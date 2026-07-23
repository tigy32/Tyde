use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::app::{connect_one_host, refresh_configured_hosts};
use crate::bridge::{self, HostTransportConfig as BridgeHostTransportConfig};
use crate::send::send_frame;
use crate::state::{AppState, DiffViewMode, NativeSettingsSaveState, ToolOutputMode};

use protocol::{
    BackendConfigField, BackendConfigFieldType, BackendConfigPersistenceMode,
    BackendConfigSnapshotStatus, BackendConfigValues, BackendKind, BackendNativeSettingsAdvisory,
    BackendNativeSettingsGroup, BackendNativeSettingsGroupKind, BackendNativeSettingsSnapshot,
    BackendSetupAction, BackendSetupInfo, BackendSetupStatus, BackgroundAgentFeature, BrokerUrl,
    CodeIntelProviderId, CustomAgent, CustomAgentId, DiffContextMode, FrameKind,
    HostExecutablePath, HostLaunchProfileConfig, HostSettingValue, LaunchProfileId,
    McpServerConfig, McpServerId, McpTransportConfig, MobileAccessStatePayload, MobileBrokerStatus,
    MobileDeviceState, MobilePairingOfferId, MobilePairingOfferPayload, MobilePairingState,
    ProjectId, RunBackendSetupPayload, SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX,
    SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN, SUPERVISOR_RETRY_ATTEMPTS_MAX,
    SUPERVISOR_RETRY_ATTEMPTS_MIN, SessionSchemaEntry, SessionSettingField,
    SessionSettingFieldType, SessionSettingValue, SessionSettingsSchema, SessionSettingsValues,
    SetSettingPayload, Skill, SkillId, Steering, SteeringId, SteeringScope, ToolPolicy,
};

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

use crate::components::backend_capacity::BackendSubscriptionCapacity;
use crate::components::session_settings::{
    SessionSettingsControls, clear_invalid_dependent_select_values,
};
use crate::send::{
    custom_agent_delete, custom_agent_upsert, mcp_server_delete, mcp_server_upsert,
    mobile_device_revoke, mobile_pairing_cancel, mobile_pairing_start, skill_refresh,
    steering_delete, steering_upsert,
};

const RESERVED_MCP_NAMES: &[&str] = &["tyde-debug", "tyde-agent-control", "tyde-review-feedback"];

/// Frontend-side mirror of the server's broker-URL acceptance rules for the
/// `mobile_broker_url` **dev override**. The server
/// (`server::mobile_access::dev_broker_endpoint`, over
/// `mqtt-transport::validate_broker_url`) is the authoritative validator; this
/// mirror gives the user immediate inline feedback instead of a value that is
/// accepted here but rejected on write.
///
/// Rules mirrored:
/// - scheme must be `mqtts://` or `wss://` (no insecure/unknown schemes);
/// - no embedded credentials (`@`) or fragments (`#`);
/// - the URL must point at a **loopback** host (`localhost`, an IPv4 loopback
///   like `127.0.0.1`, or the `[::1]` IPv6 loopback). Custom broker URLs are
///   dev/test-only; the public default and any other host are rejected because
///   production mobile access uses tycode.dev-managed AWS IoT.
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
        // Dev-override loopback rule — matches the server, which fails closed
        // for public/free/custom production brokers.
        if !broker_url_host(after_scheme)
            .as_deref()
            .is_some_and(is_loopback_host)
        {
            return Err(
                "Custom broker URLs are dev/test-only and must be a loopback host (localhost / 127.0.0.1). Leave blank for tycode.dev-managed access.",
            );
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

/// Extracts the host from the part of a broker URL after `://`. The server
/// parses the URL with the `url` crate and applies the same loopback check to
/// `url::Url::host()`; this string extraction yields the same host for the
/// broker URLs the field accepts. Callers have already rejected embedded
/// credentials (`@`) and fragments (`#`). Returns `None` when no host is present.
fn broker_url_host(after_scheme: &str) -> Option<String> {
    // Authority is everything before the first path/query separator.
    let authority = after_scheme.split(['/', '?']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    // IPv6 literal: "[::1]:8883" -> host "::1".
    if let Some(rest) = authority.strip_prefix('[') {
        return rest
            .split_once(']')
            .map(|(host, _)| host.to_owned())
            .filter(|host| !host.is_empty());
    }
    // "host" or "host:port" -> host up to the first ':'.
    let host = authority.split(':').next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_owned())
}

/// Mirror of the server's `is_loopback_url` host check: `localhost` (case
/// insensitive) or any IP literal whose address is a loopback address (covers
/// `127.0.0.0/8` and `::1`).
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|addr| addr.is_loopback())
            .unwrap_or(false)
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
        MobileBrokerStatus::RepairRequired { message, .. } => {
            format!("Repair required: {message}")
        }
    }
}

/// Slug used by CSS to pick a per-state color for the broker pill.
fn broker_status_slug(status: &MobileBrokerStatus) -> &'static str {
    match status {
        MobileBrokerStatus::Disabled => "disabled",
        MobileBrokerStatus::Connecting { .. } => "connecting",
        MobileBrokerStatus::Online { .. } => "online",
        MobileBrokerStatus::Error { .. } => "error",
        MobileBrokerStatus::RepairRequired { .. } => "error",
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
        MobilePairingState::RepairRequired { message, .. } => {
            Some(format!("Repair required: {message}"))
        }
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
    // These tests mirror the rules the server enforces for the dev-override
    // `mobile_broker_url` (`server::mobile_access::dev_broker_endpoint` over
    // `mqtt-transport::validate_broker_url`): secure scheme, no embedded
    // credentials/fragments, and a loopback-only host. The server remains the
    // authoritative validator; this mirror surfaces the same rejection inline.

    #[test]
    fn broker_url_validator_accepts_empty() {
        // Empty input = "use managed access", not an error.
        assert!(validate_broker_url_input("").is_ok());
        assert!(validate_broker_url_input("   ").is_ok());
    }

    #[test]
    fn broker_url_validator_accepts_loopback_hosts() {
        // Loopback dev overrides are the only accepted custom brokers.
        assert!(validate_broker_url_input("mqtts://localhost:8883").is_ok());
        assert!(validate_broker_url_input("wss://127.0.0.1:8083/mqtt").is_ok());
        assert!(validate_broker_url_input("wss://localhost/relay").is_ok());
        // IPv6 loopback literal.
        assert!(validate_broker_url_input("mqtts://[::1]:8883").is_ok());
        // Case-insensitive on scheme and on the `localhost` host.
        assert!(validate_broker_url_input("MQTTS://LOCALHOST:8883").is_ok());
    }

    #[test]
    fn broker_url_validator_rejects_non_loopback_custom_broker() {
        // A valid-scheme, valid-shape URL at a non-loopback host must be
        // rejected inline — the server fails closed for it.
        for bad in [
            "mqtts://broker.example.test:8883",
            "wss://broker.emqx.io:8084/mqtt",
            "wss://192.168.1.10:8083/mqtt",
            "mqtts://10.0.0.5:8883",
        ] {
            let err = validate_broker_url_input(bad)
                .expect_err(&format!("expected non-loopback {bad:?} to be rejected"));
            assert!(
                err.contains("loopback"),
                "error for {bad:?} must explain the loopback rule: {err}"
            );
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab {
    Hosts,
    Appearance,
    General,
    Supervisor,
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
            Self::Supervisor => "Supervisor",
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
            Self::Supervisor => &[
                "Supervisor",
                "Agent supervisor",
                "Enable agent supervisor",
                "Auto-compact on success",
                "Verdict model tier",
                "Low",
                "High",
                "Backend default",
                "Kick limit",
                "Extra delayed attempts",
                "Idle",
                "Continue",
                "Task list",
                "Compaction",
            ],
            Self::Backends => &[
                "Backends",
                "Overview",
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
                "Managed access",
                "tycode.dev",
                "AWS IoT",
                "Broker URL",
                "Tyggs Pass",
                "Repair",
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

const ALL_TABS: [SettingsTab; 11] = [
    SettingsTab::Hosts,
    SettingsTab::Appearance,
    SettingsTab::General,
    SettingsTab::Supervisor,
    SettingsTab::Backends,
    SettingsTab::CustomAgents,
    SettingsTab::McpServers,
    SettingsTab::Steering,
    SettingsTab::Skills,
    SettingsTab::Mobile,
    SettingsTab::Debug,
];

/// Tabs listed under the "Settings" sidebar group. `SettingsTab::Backends` is
/// deliberately absent: it renders as the stable "Overview" entry of the
/// dedicated Backends group.
const SETTINGS_GROUP_TABS: [SettingsTab; 10] = [
    SettingsTab::Hosts,
    SettingsTab::Appearance,
    SettingsTab::General,
    SettingsTab::Supervisor,
    SettingsTab::CustomAgents,
    SettingsTab::McpServers,
    SettingsTab::Steering,
    SettingsTab::Skills,
    SettingsTab::Mobile,
    SettingsTab::Debug,
];

/// One page of the settings panel: either a regular tab or a per-backend
/// settings page derived from the server-owned backend-config schema catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    Tab(SettingsTab),
    Complexity,
    Backend(BackendKind),
}

/// Backends that get their own sidebar page on the selected host, in the
/// canonical backend order. Derived purely from server-owned state — never
/// from `enabled_backends`, and never hardcoded per backend. A backend earns a
/// page if it exposes a typed deep-config schema *or* the server has published
/// a backend-native settings snapshot for it (e.g. Tycode's grouped settings).
fn schema_backends(state: &AppState) -> Vec<BackendKind> {
    let Some(host_id) = state.selected_host_id.get() else {
        return Vec::new();
    };
    let schemas = state.backend_config_schemas.get();
    let host_schemas = schemas.get(&host_id);
    let native = state.backend_native_settings.get();
    let host_native = native.get(&host_id);
    all_backends()
        .into_iter()
        .filter(|kind| {
            let has_schema = host_schemas
                .and_then(|m| m.get(kind))
                .is_some_and(|schema| !schema.fields.is_empty());
            let has_native = host_native.is_some_and(|m| m.contains_key(kind));
            has_schema || has_native
        })
        .collect()
}

/// Search matching for a per-backend page: the backend's name plus the
/// server-provided schema field labels/descriptions and native settings group
/// titles/descriptions.
fn backend_page_matches_query(state: &AppState, kind: BackendKind, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    if backend_label(kind).to_lowercase().contains(&q) {
        return true;
    }
    let Some(host_id) = state.selected_host_id.get() else {
        return false;
    };
    let schemas = state.backend_config_schemas.get();
    if let Some(schema) = schemas.get(&host_id).and_then(|m| m.get(&kind))
        && schema.fields.iter().any(|field| {
            field.label.to_lowercase().contains(&q)
                || field
                    .description
                    .as_ref()
                    .is_some_and(|d| d.to_lowercase().contains(&q))
        })
    {
        return true;
    }
    let native = state.backend_native_settings.get();
    if let Some(snapshot) = native.get(&host_id).and_then(|m| m.get(&kind)) {
        return snapshot.groups.iter().any(|group| {
            group.title.to_lowercase().contains(&q)
                || group
                    .description
                    .as_ref()
                    .is_some_and(|d| d.to_lowercase().contains(&q))
        });
    }
    false
}

#[component]
pub fn SettingsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_page = RwSignal::new(SettingsPage::Tab(SettingsTab::Appearance));
    let search_query = RwSignal::new(String::new());

    // Honor deep-link requests (e.g. the onboarding "Set up an AI engine" CTA
    // asking to open straight to the Backends tab).
    {
        let state = state.clone();
        Effect::new(move |_| {
            if let Some(label) = state.settings_tab_request.get() {
                if let Some(tab) = ALL_TABS.into_iter().find(|tab| tab.label() == label) {
                    active_page.set(SettingsPage::Tab(tab));
                }
                state.settings_tab_request.set(None);
            }
        });
    }

    // A backend page only exists while the selected host's schema catalog
    // carries that backend. If the host changes (or schemas haven't loaded),
    // fall back to the stable Overview page instead of rendering a stale or
    // blank child page.
    {
        let state = state.clone();
        Effect::new(move |_| {
            if let SettingsPage::Backend(kind) = active_page.get()
                && !schema_backends(&state).contains(&kind)
            {
                active_page.set(SettingsPage::Tab(SettingsTab::Backends));
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
                            <Show when=move || {
                                SETTINGS_GROUP_TABS
                                    .into_iter()
                                    .any(|tab| tab.matches_query(&search_query.get()))
                            }>
                                <div class="settings-nav-group">
                                    <div class="settings-nav-group-title">"Settings"</div>
                                    <div class="settings-nav-group-items">
                                        {SETTINGS_GROUP_TABS.map(|tab| {
                                            let is_active = move || {
                                                active_page.get() == SettingsPage::Tab(tab)
                                            };
                                            let matches_search = move || {
                                                tab.matches_query(&search_query.get())
                                            };
                                            view! {
                                                <Show when=matches_search>
                                                    <button
                                                        class="settings-nav-item"
                                                        class:active=is_active
                                                        on:click=move |_| {
                                                            active_page.set(SettingsPage::Tab(tab))
                                                        }
                                                    >
                                                        {tab.label()}
                                                    </button>
                                                </Show>
                                            }
                                        }).collect_view()}
                                    </div>
                                </div>
                            </Show>
                            <BackendsNavGroup active_page search_query />
                            <div class="settings-nav-footer">
                                <button class="settings-feedback-link" on:click=move |_| {
                                    state.settings_open.set(false);
                                    state.feedback_open.set(true);
                                }>"Send Feedback"</button>
                            </div>
                        </nav>

                        <div class="settings-content">
                            {move || match active_page.get() {
                                SettingsPage::Tab(tab) => match tab {
                                    SettingsTab::Hosts => view! { <HostsTab /> }.into_any(),
                                    SettingsTab::Appearance => view! { <AppearanceTab /> }.into_any(),
                                    SettingsTab::General => view! { <GeneralTab /> }.into_any(),
                                    SettingsTab::Supervisor => view! { <SupervisorTab /> }.into_any(),
                                    SettingsTab::Backends => view! { <BackendsTab active_page /> }.into_any(),
                                    SettingsTab::CustomAgents => view! { <CustomAgentsTab /> }.into_any(),
                                    SettingsTab::McpServers => view! { <McpServersTab /> }.into_any(),
                                    SettingsTab::Steering => view! { <SteeringTab /> }.into_any(),
                                    SettingsTab::Skills => view! { <SkillsTab /> }.into_any(),
                                    SettingsTab::Mobile => view! { <MobileTab /> }.into_any(),
                                    SettingsTab::Debug => view! { <DebugTab /> }.into_any(),
                                },
                                SettingsPage::Backend(kind) => {
                                    view! { <BackendSettingsPage kind /> }.into_any()
                                }
                                SettingsPage::Complexity => {
                                    view! { <TaskComplexityPage /> }.into_any()
                                }
                            }}
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// The "Backends" sidebar group: a stable Overview entry plus one page per
/// backend in the selected host's server-owned schema catalog.
#[component]
fn BackendsNavGroup(
    active_page: RwSignal<SettingsPage>,
    search_query: RwSignal<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let visible = move || {
        let query = search_query.get();
        SettingsTab::Backends.matches_query(&query)
            || complexity_page_matches_query(&query)
            || schema_backends(&state)
                .into_iter()
                .any(|kind| backend_page_matches_query(&state, kind, &query))
    };
    view! {
        <Show when=visible>
            <div class="settings-nav-group">
                <div class="settings-nav-group-title">"Backends"</div>
                <div class="settings-nav-group-items">
                    <Show when=move || SettingsTab::Backends.matches_query(&search_query.get())>
                        <button
                            class="settings-nav-item"
                            class:active=move || {
                                active_page.get() == SettingsPage::Tab(SettingsTab::Backends)
                            }
                            on:click=move |_| {
                                active_page.set(SettingsPage::Tab(SettingsTab::Backends))
                            }
                        >
                            "Overview"
                        </button>
                    </Show>
                    <Show when=move || complexity_page_matches_query(&search_query.get())>
                        <button
                            class="settings-nav-item"
                            class:active=move || active_page.get() == SettingsPage::Complexity
                            on:click=move |_| active_page.set(SettingsPage::Complexity)
                        >
                            "Task Complexity"
                        </button>
                    </Show>
                    <BackendNavItems active_page search_query />
                </div>
            </div>
        </Show>
    }
}

#[component]
fn BackendNavItems(
    active_page: RwSignal<SettingsPage>,
    search_query: RwSignal<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    view! {
        {move || {
            let query = search_query.get();
            schema_backends(&state)
                .into_iter()
                .filter(|kind| backend_page_matches_query(&state, *kind, &query))
                .map(|kind| {
                    view! {
                        <button
                            class="settings-nav-item"
                            class:active=move || active_page.get() == SettingsPage::Backend(kind)
                            on:click=move |_| active_page.set(SettingsPage::Backend(kind))
                        >
                            {backend_label(kind)}
                        </button>
                    }
                })
                .collect_view()
        }}
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

/// Agent supervisor settings. When an agent goes idle, a hidden background
/// model call reviews the last user request, the task list, and the agent's
/// final message, then classifies it as done, awaiting user input, or needing
/// a follow-up that kicks the agent back to work. Every knob is host-scoped and
/// committed as a typed `Supervisor*` host setting; the feature costs money
/// per idle transition, so it defaults off.
#[component]
fn SupervisorTab() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_enabled_checked = state.clone();
    let state_for_enabled_disabled = state.clone();
    let state_for_compact_checked = state.clone();
    let state_for_compact_disabled = state.clone();
    let state_for_compact_delay_value = state.clone();
    let state_for_compact_delay_disabled = state.clone();
    let state_for_compact_min_value = state.clone();
    let state_for_compact_min_disabled = state.clone();
    let state_for_kicks_value = state.clone();
    let state_for_kicks_disabled = state.clone();
    let state_for_retries_value = state.clone();
    let state_for_retries_disabled = state.clone();

    let enabled_checked = move || {
        state_for_enabled_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.supervisor.enabled)
    };
    let enabled_disabled = move || {
        state_for_enabled_disabled
            .selected_host_settings()
            .is_none()
    };
    let enabled_on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::SupervisorEnabled {
                    enabled: input.checked(),
                },
            );
        }
    };

    let compact_checked = move || {
        state_for_compact_checked
            .selected_host_settings()
            .is_some_and(|settings| settings.supervisor.auto_compact_on_success)
    };
    let compact_disabled = move || {
        state_for_compact_disabled
            .selected_host_settings()
            .is_none_or(|settings| !settings.supervisor.enabled)
    };

    let compact_delay_value = move || {
        state_for_compact_delay_value
            .selected_host_settings()
            .map(|settings| {
                settings
                    .supervisor
                    .auto_compact_inactivity_delay_seconds
                    .to_string()
            })
            .unwrap_or_default()
    };
    let compact_delay_disabled = move || {
        state_for_compact_delay_disabled
            .selected_host_settings()
            .is_none_or(|settings| {
                !settings.supervisor.enabled || !settings.supervisor.auto_compact_on_success
            })
    };
    let compact_delay_on_change = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            if let Ok(seconds) = input.value().trim().parse::<u32>()
                && (SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN
                    ..=SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX)
                    .contains(&seconds)
            {
                send_host_setting(
                    &state,
                    HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds },
                );
            }
        }
    };
    let compact_on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            send_host_setting(
                &state,
                HostSettingValue::SupervisorAutoCompactOnSuccess {
                    enabled: input.checked(),
                },
            );
        }
    };

    let compact_min_value = move || {
        state_for_compact_min_value
            .selected_host_settings()
            .map(|settings| {
                settings
                    .supervisor
                    .auto_compact_min_context_tokens
                    .to_string()
            })
            .unwrap_or_default()
    };
    let compact_min_disabled = move || {
        state_for_compact_min_disabled
            .selected_host_settings()
            .is_none_or(|settings| {
                !settings.supervisor.enabled || !settings.supervisor.auto_compact_on_success
            })
    };
    let compact_min_on_change = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            if let Ok(tokens) = input.value().trim().parse::<u64>() {
                send_host_setting(
                    &state,
                    HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens },
                );
            }
        }
    };

    let kicks_value = move || {
        state_for_kicks_value
            .selected_host_settings()
            .map(|settings| settings.supervisor.max_kicks_per_task.to_string())
            .unwrap_or_default()
    };
    let kicks_disabled = move || state_for_kicks_disabled.selected_host_settings().is_none();
    let kicks_on_change = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            if let Ok(count) = input.value().trim().parse::<u8>()
                && count >= 1
            {
                send_host_setting(
                    &state,
                    HostSettingValue::SupervisorMaxKicksPerTask { count },
                );
            }
        }
    };

    let state_for_tier_value = state.clone();
    let state_for_tier_disabled = state.clone();
    let tier_value = move || {
        let tier = state_for_tier_value
            .selected_host_settings()
            .map(|settings| settings.supervisor.cost_tier)
            .unwrap_or_default();
        match tier {
            protocol::SupervisorCostTier::Low => "low",
            protocol::SupervisorCostTier::Default => "default",
            protocol::SupervisorCostTier::High => "high",
        }
        .to_owned()
    };
    let tier_disabled = move || state_for_tier_disabled.selected_host_settings().is_none();
    let tier_on_change = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let select: web_sys::HtmlSelectElement = target.unchecked_into();
            let tier = match select.value().as_str() {
                "low" => protocol::SupervisorCostTier::Low,
                "default" => protocol::SupervisorCostTier::Default,
                "high" => protocol::SupervisorCostTier::High,
                other => {
                    log::warn!("unknown supervisor cost tier option: {other:?}");
                    return;
                }
            };
            send_host_setting(&state, HostSettingValue::SupervisorCostTier { tier });
        }
    };

    let retries_value = move || {
        state_for_retries_value
            .selected_host_settings()
            .map(|settings| settings.supervisor.retry_attempts.to_string())
            .unwrap_or_default()
    };
    let retries_disabled = move || {
        state_for_retries_disabled
            .selected_host_settings()
            .is_none()
    };
    let retries_on_change = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            if let Ok(count) = input.value().trim().parse::<u8>()
                && count <= SUPERVISOR_RETRY_ATTEMPTS_MAX
            {
                send_host_setting(&state, HostSettingValue::SupervisorRetryAttempts { count });
            }
        }
    };

    view! {
        <h2 class="settings-panel-title">"Supervisor"</h2>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Enable agent supervisor"</label>
                    <p class="settings-description">
                        "When an agent goes idle, a background model call checks whether it actually finished the last request. If the agent stopped on an error or quit mid-task, the supervisor sends it a follow-up message to keep it working. This runs an extra model call per idle transition and costs money. Off by default."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=enabled_checked
                        disabled=enabled_disabled
                        on:change=enabled_on_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Auto-compact on success"</label>
                    <p class="settings-description">
                        "After the supervisor confirms the requested work is truly complete, automatically compact only after the inactivity delay has elapsed and the latest completed assistant turn reports a known current context strictly above the configured minimum."
                    </p>
                </div>
                <label class="settings-toggle">
                    <input
                        type="checkbox"
                        prop:checked=compact_checked
                        disabled=compact_disabled
                        on:change=compact_on_toggle
                    />
                    <span class="settings-toggle-slider"></span>
                </label>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Auto-compact inactivity delay"</label>
            <p class="settings-description">
                "Default 300 seconds (5 minutes); agent activity restarts the timer. Compaction starts only after true completion, this full quiet period, and a known current context strictly above the configured minimum."
            </p>
            <div class="settings-form-row" style="align-items: center;">
                <input
                    class="settings-input settings-supervisor-number-input"
                    type="number"
                    min=SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MIN
                    max=SUPERVISOR_AUTO_COMPACT_INACTIVITY_DELAY_SECONDS_MAX
                    step="1"
                    prop:value=compact_delay_value
                    disabled=compact_delay_disabled
                    aria-label="Supervisor auto-compact inactivity delay seconds"
                    on:change=compact_delay_on_change
                />
                <span class="settings-supervisor-number-unit">"seconds"</span>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Auto-compact minimum context"</label>
            <p class="settings-description">
                "Automatically compact only when the latest completed assistant turn reports a context larger than this many tokens. The default is 200,000. Set 0 for no positive minimum; automatic compaction still waits for reported current-context data and skips the turn when that data is unavailable."
            </p>
            <div class="settings-form-row" style="align-items: center;">
                <input
                    class="settings-input settings-supervisor-number-input"
                    type="number"
                    min="0"
                    step="1000"
                    prop:value=compact_min_value
                    disabled=compact_min_disabled
                    aria-label="Supervisor auto-compact minimum context tokens"
                    on:change=compact_min_on_change
                />
                <span class="settings-supervisor-number-unit">"tokens"</span>
            </div>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Verdict model tier"</label>
            <p class="settings-description">
                "Which model tier judges whether a task is finished. Low uses the cheap tier (like agent naming); Backend default uses the backend's normal model; High uses the most capable configuration. Raise this if the supervisor's verdicts are unreliable."
            </p>
            <select
                class="settings-select"
                prop:value=tier_value
                disabled=tier_disabled
                aria-label="Supervisor verdict model tier"
                on:change=tier_on_change
            >
                <option value="low">"Low (cheap)"</option>
                <option value="default">"Backend default"</option>
                <option value="high">"High (most capable)"</option>
            </select>
        </div>

        <div class="settings-field">
            <label class="settings-label">"Kick limit"</label>
            <p class="settings-description">
                "Maximum consecutive supervisor follow-ups without a new message from you. Prevents the supervisor and the agent from looping forever on a task that cannot finish."
            </p>
            <input
                class="settings-input settings-supervisor-number-input"
                type="number"
                min="1"
                max="20"
                prop:value=kicks_value
                disabled=kicks_disabled
                aria-label="Supervisor kick limit"
                on:change=kicks_on_change
            />
        </div>

        <div class="settings-field">
            <label class="settings-label">"Extra delayed attempts"</label>
            <p class="settings-description">
                "Extra delayed attempts after a supervisor verdict call fails or returns an invalid verdict. Each attempt is a fresh paid model call with automatic backoff. 0 disables extra attempts; maximum 5. The default 1 means two total calls."
            </p>
            <input
                class="settings-input settings-supervisor-number-input"
                type="number"
                min=SUPERVISOR_RETRY_ATTEMPTS_MIN
                max=SUPERVISOR_RETRY_ATTEMPTS_MAX
                prop:value=retries_value
                disabled=retries_disabled
                aria-label="Supervisor extra delayed attempts"
                on:change=retries_on_change
            />
        </div>
    }
}

#[component]
fn BackendsTab(active_page: RwSignal<SettingsPage>) -> impl IntoView {
    let state = expect_context::<AppState>();

    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Backends"</h2>
        </div>

        <p class="settings-description settings-panel-intro">
            "Toggle backends, install them on the selected host, and run sign-in when available. Install and sign-in commands run in the host terminal so output stays visible. Backend-specific settings live on each backend's own page in the sidebar."
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
                    .map(|kind| view! { <BackendCard kind active_page /> })
                    .collect::<Vec<_>>()}
            </div>
        </div>

        <LaunchProfilesSection />
    }
}

fn complexity_page_matches_query(query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    [
        "Task Complexity",
        "Task complexity tiers",
        "Low tier",
        "High tier",
        "Cheaper faster setup",
        "Most capable setup",
    ]
    .iter()
    .any(|text| text.to_lowercase().contains(&query))
}

#[component]
fn TaskComplexityPage() -> impl IntoView {
    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">"Task Complexity"</h2>
        </div>
        <p class="settings-description settings-panel-intro">
            "Choose whether agent and spawn flows can request low- or high-complexity backend configurations."
        </p>
        <ComplexityTiersSection />
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
                    SessionSettingFieldType::Select { .. } => Some(field.clone()),
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
    fields: &[SessionSettingField],
) -> AnyView {
    let selects = fields
        .iter()
        .map(|field| {
            let current = match values.0.get(&field.key) {
                Some(SessionSettingValue::String(value)) => value.clone(),
                _ => String::new(),
            };
            let option_views = field
                .select_options(values)
                .unwrap_or_default()
                .iter()
                .map(|option| {
                    view! { <option value=option.value.clone()>{option.label.clone()}</option> }
                })
                .collect::<Vec<_>>();
            let state = state.clone();
            let key = field.key.clone();
            let fields = fields.to_vec();
            view! {
                <label class="settings-tier-select">
                    <span class="settings-tier-select-label">{field.label.clone()}</span>
                    <select
                        class="settings-select"
                        prop:value=current
                        on:change=move |ev: web_sys::Event| {
                            let target = ev.target().unwrap();
                            let el: web_sys::HtmlSelectElement = target.unchecked_into();
                            update_tier_setting(
                                &state,
                                kind,
                                is_high,
                                &key,
                                el.value(),
                                &fields,
                            );
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
    fields: &[SessionSettingField],
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
    clear_invalid_dependent_select_values(fields, tier_values);
    send_host_setting(
        state,
        HostSettingValue::BackendTiers {
            backend: kind,
            config,
        },
    );
}

/// One backend's settings page, reached from the Backends sidebar group. The
/// page content is driven entirely by the server-owned schema, snapshot, and
/// host-settings state for the selected host; fields are generated from the
/// backend's `BackendConfigSchema`, so a backend controls exactly which fields
/// appear here with no frontend changes.
#[component]
fn BackendSettingsPage(kind: BackendKind) -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_body = state.clone();
    let state_for_status = state.clone();
    // The selected native-settings group (by id) lives at the page level, not
    // inside the body closure, so it survives the body's reactive rerenders
    // (a save marking `native_settings_save_state` Pending, or a fresh snapshot
    // arriving) instead of snapping the user back to the Core tab each time.
    // `None` means "use the first/Core group"; it's not backend state, just this
    // page's view selection, and it resets when the user navigates to a
    // different backend page (a new `BackendSettingsPage` instance).
    let active_native_group = RwSignal::new(Option::<String>::None);
    // Which Tycode settings profile is being edited. Same lifecycle rationale
    // as `active_native_group`: page-local view selection that must survive
    // body rebuilds on snapshot/save-state changes.
    let active_native_profile = RwSignal::new(Option::<String>::None);
    let setup_info = move || {
        state_for_status
            .selected_host_backend_setup()
            .and_then(|infos| infos.into_iter().find(|info| info.backend_kind == kind))
    };
    let setup_info_for_class = setup_info.clone();
    let state_for_intro = state.clone();
    // The intro states where edits land, straight from the schema's
    // server-owned persistence mode — never inferred per backend.
    let intro = move || {
        let mode = state_for_intro.selected_host_id.get().and_then(|host_id| {
            state_for_intro
                .backend_config_schemas
                .get()
                .get(&host_id)
                .and_then(|m| m.get(&kind))
                .map(|schema| schema.persistence_mode)
        });
        match mode {
            Some(BackendConfigPersistenceMode::BackendNative) => {
                "Settings are written to the backend's own configuration on the selected host. Editing a field saves an explicit Tyde override that applies to every new session; clearing it restores the backend's own value."
            }
            Some(BackendConfigPersistenceMode::TydeSettingsStore) => {
                "Settings are stored in Tyde on the selected host and applied to every new session. Editing a field saves an explicit Tyde override; clearing it restores the backend's own value."
            }
            None => "Settings for this backend on the selected host.",
        }
    };

    view! {
        <div class="settings-panel-header">
            <h2 class="settings-panel-title">{backend_label(kind)}</h2>
            <span class=move || backend_setup_status_class(setup_info_for_class().as_ref())>
                {move || backend_setup_status_label(setup_info().as_ref())}
            </span>
        </div>

        <p class="settings-description settings-panel-intro">{intro}</p>

        {move || backend_page_body(&state_for_body, kind, active_native_group, active_native_profile)}
    }
}

fn backend_page_body(
    state: &AppState,
    kind: BackendKind,
    active_native_group: RwSignal<Option<String>>,
    active_native_profile: RwSignal<Option<String>>,
) -> AnyView {
    let Some(host_id) = state.selected_host_id.get() else {
        return view! {
            <p class="settings-description">"Select a host to configure this backend."</p>
        }
        .into_any();
    };
    let Some(settings) = state.selected_host_settings() else {
        return view! {
            <p class="settings-description">"Host settings not loaded for the selected host."</p>
        }
        .into_any();
    };
    let schemas = state.backend_config_schemas.get();
    let Some(schema) = schemas.get(&host_id).and_then(|m| m.get(&kind)).cloned() else {
        // No typed deep-config schema. Hermes gets a bespoke page driven by
        // the typed `HermesNativeSettingsDoc` inside its backend-native
        // snapshot. Returning before the reactive `backend_native_settings`
        // read below is deliberate: the Hermes page subscribes to the snapshot
        // itself, so a snapshot republish (e.g. after a save) rerenders only
        // that page's body — this outer closure, and with it the page's local
        // edit state, survives.
        if kind == BackendKind::Hermes {
            return crate::components::hermes_settings::hermes_settings_page_body(&host_id);
        }
        // Other backends may instead publish a backend-native settings
        // snapshot (e.g. Tycode's grouped settings) — render that when
        // present. Otherwise this is the transient window the nav fallback
        // effect handles by returning to Overview.
        if let Some(snapshot) = state
            .backend_native_settings
            .get()
            .get(&host_id)
            .and_then(|m| m.get(&kind))
            .cloned()
        {
            return backend_native_settings_body(
                state,
                kind,
                &snapshot,
                active_native_group,
                active_native_profile,
            );
        }
        return view! {
            <p class="settings-description">
                "No configuration is available for this backend on the selected host."
            </p>
        }
        .into_any();
    };

    // Pages are never hidden for disabled or uninstalled backends — instead
    // the state is explicit and the controls lock until the backend can
    // actually accept edits. The schema's server-owned persistence mode says
    // what an edit needs: `BackendNative` config is written straight to the
    // backend's own configuration source, so those edits also require the
    // backend to be installed and runnable; `TydeSettingsStore` config lives
    // in Tyde host settings and stays editable whenever the backend is
    // enabled — users may need those settings precisely to recover a backend
    // whose setup probe reports it unavailable.
    let enabled = settings.enabled_backends.contains(&kind);
    let setup_status = state
        .selected_host_backend_setup()
        .and_then(|infos| infos.into_iter().find(|info| info.backend_kind == kind))
        .map(|info| info.status);
    let needs_install = match schema.persistence_mode {
        BackendConfigPersistenceMode::BackendNative => {
            setup_status != Some(BackendSetupStatus::Installed)
        }
        BackendConfigPersistenceMode::TydeSettingsStore => false,
    };
    let locked = !enabled || needs_install;
    let locked_banner = locked.then(|| {
        let mut reasons: Vec<String> = Vec::new();
        if !enabled {
            reasons.push(format!(
                "{} is disabled on the selected host, so it isn't offered for new chats.",
                backend_label(kind),
            ));
        }
        if needs_install {
            match setup_status {
                Some(BackendSetupStatus::Installed) => {}
                Some(BackendSetupStatus::NotInstalled) => reasons.push(format!(
                    "{} is not installed on this host. Install it from the Backends overview.",
                    backend_label(kind),
                )),
                Some(BackendSetupStatus::Unavailable) => reasons.push(format!(
                    "{} is currently unavailable on this host. See the Backends overview for details.",
                    backend_label(kind),
                )),
                Some(BackendSetupStatus::Unsupported) => reasons.push(format!(
                    "Automatic setup for {} is not supported on this host platform.",
                    backend_label(kind),
                )),
                None => reasons.push("Checking install status for this host…".to_owned()),
            }
        }
        let mut requirements = Vec::new();
        if !enabled {
            requirements.push("enabled");
        }
        if needs_install {
            requirements.push("installed");
        }
        reasons.push(format!(
            "Settings are read-only until the backend is {}.",
            requirements.join(" and "),
        ));
        let enable_button = (!enabled).then(|| {
            let state_for_enable = state.clone();
            let enabled_now = settings.enabled_backends.clone();
            view! {
                <button
                    class="settings-btn settings-btn-primary"
                    on:click=move |_| {
                        let enabled_backends = all_backends()
                            .into_iter()
                            .filter(|candidate| {
                                *candidate == kind || enabled_now.contains(candidate)
                            })
                            .collect::<Vec<_>>();
                        send_host_setting(
                            &state_for_enable,
                            HostSettingValue::EnabledBackends { enabled_backends },
                        );
                    }
                >
                    "Enable backend"
                </button>
            }
        });
        view! {
            <div class="settings-backend-page-banner">
                <p class="settings-backend-page-banner-text">{reasons.join(" ")}</p>
                {enable_button}
            </div>
        }
    });

    if schema.fields.is_empty() {
        return view! {
            {locked_banner}
            <p class="settings-description">"This backend has no configurable settings."</p>
        }
        .into_any();
    }

    let values = settings
        .backend_config
        .get(&kind)
        .cloned()
        .unwrap_or_default();
    let snapshots = state.backend_config_snapshots.get();
    let snapshot = snapshots.get(&host_id).and_then(|m| m.get(&kind));
    // Backend-native current values, only when the server could actually
    // read them. Never invented client-side.
    let native = snapshot
        .filter(|s| s.status == BackendConfigSnapshotStatus::Ready)
        .map(|s| s.values.clone())
        .unwrap_or_default();
    // The server owns field order, so render the first field as the page's
    // emphasized primary control (Tycode → Active Provider, Hermes → Default
    // Model) and the rest as a secondary grid. Emphasis follows schema order,
    // not any hard-coded key name.
    let fields = schema
        .fields
        .iter()
        .enumerate()
        .map(|(idx, field)| {
            backend_config_field(state, kind, field, &values, &native, idx == 0, locked)
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

    view! {
        {locked_banner}
        {snapshot_note}
        <div class="settings-backend-config-fields">{fields}</div>
    }
    .into_any()
}

fn backend_config_field(
    state: &AppState,
    kind: BackendKind,
    field: &BackendConfigField,
    values: &BackendConfigValues,
    native: &BackendConfigValues,
    primary: bool,
    locked: bool,
) -> AnyView {
    let key = field.key.clone();
    let description = field.description.clone();

    // `disabled` already blocks user interaction; the handler guards exist so
    // a locked field can never reach the wire even via synthetic events.
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
                if locked {
                    return;
                }
                let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                commit_text_value(&state, kind, &key_for_change, el.value());
            };
            if *multiline {
                view! {
                    <textarea
                        class="settings-input settings-backend-config-input"
                        prop:value=current
                        placeholder=placeholder
                        disabled=locked
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
                        disabled=locked
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
                if locked {
                    return;
                }
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
                    disabled=locked
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
                if locked {
                    return;
                }
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
                <select
                    class="settings-select"
                    prop:value=current
                    disabled=locked
                    on:change=on_change
                >
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
                if locked {
                    return;
                }
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
                    <input
                        type="checkbox"
                        prop:checked=current
                        disabled=locked
                        on:change=on_change
                    />
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
                if locked {
                    return;
                }
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
                    disabled=locked
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

// ---- Backend-native, JSON-schema-driven settings (e.g. Tycode) ----

/// Secret-like key markers. Native settings groups carry raw JSON schema, which
/// is not guaranteed to flag secrets in a typed way, so mask defensively by key
/// name and by the JSON-schema hints a backend might set.
const NATIVE_SECRET_MARKERS: [&str; 6] = [
    "api_key",
    "apikey",
    "password",
    "secret",
    "token",
    "access_key",
];

/// Placeholder shown in place of any redacted secret value in JSON views.
const SECRET_REDACTION: &str = "••••••••";

/// Whether a key *name* alone marks a secret. Used for recursive redaction of
/// nested JSON where no per-key schema is available.
fn is_secret_key_name(key: &str) -> bool {
    let lowered = key.to_lowercase();
    NATIVE_SECRET_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Whether a native settings property is a secret whose value must be masked and
/// never rendered. Considers the key name plus JSON-schema secret hints.
fn is_secret_native_key(key: &str, prop_schema: &Value) -> bool {
    is_secret_key_name(key)
        || prop_schema.get("format").and_then(Value::as_str) == Some("password")
        || prop_schema.get("writeOnly").and_then(Value::as_bool) == Some(true)
}

/// Whether `value` contains any secret-like key at any depth.
fn contains_secret(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, child)| is_secret_key_name(key) || contains_secret(child)),
        Value::Array(items) => items.iter().any(contains_secret),
        _ => false,
    }
}

/// A copy of `value` with every secret-like key's value replaced by a redaction
/// marker, recursively. Never exposes a stored secret in a rendered JSON view.
fn redact_secrets(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    let redacted = if is_secret_key_name(key) {
                        Value::String(SECRET_REDACTION.to_owned())
                    } else {
                        redact_secrets(child)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_secrets).collect()),
        other => other.clone(),
    }
}

/// The primitive JSON-schema type for a property, unwrapping nullable type
/// arrays like `["string", "null"]` to the first non-null type so nullable
/// fields still render a typed control instead of falling through to raw JSON.
fn native_primitive_type(prop_schema: &Value) -> Option<String> {
    match prop_schema.get("type") {
        Some(Value::String(single)) => Some(single.clone()),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|candidate| *candidate != "null")
            .map(str::to_owned),
        _ => None,
    }
}

fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Reactive save indicator for a backend's native settings on the selected host,
/// derived from server-owned state. Returns `(saving, error)`: `saving` is true
/// while a save is in flight (a `Pending` save whose base still equals the
/// current server settings document, i.e. the server hasn't published the result
/// yet); `error` carries the last failed-save reason.
fn native_save_indicator(
    state: &AppState,
    kind: BackendKind,
    settings: &Value,
) -> (bool, Option<String>) {
    let Some(host_id) = state.selected_host_id.get() else {
        return (false, None);
    };
    match state
        .native_settings_save_state
        .get()
        .get(&host_id)
        .and_then(|m| m.get(&kind))
    {
        Some(NativeSettingsSaveState::Pending { base }) => (base == settings, None),
        Some(NativeSettingsSaveState::Failed { message }) => (false, Some(message.clone())),
        None => (false, None),
    }
}

/// Sub-value of the settings document that a group edits: the whole document
/// when `path` is empty, else the nested value at `path`.
fn native_value_at_path<'a>(settings: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cursor = settings;
    for segment in path {
        cursor = cursor.get(segment)?;
    }
    Some(cursor)
}

/// The freshest full settings document for a backend's native snapshot, read
/// untracked (edit-time read, not a reactive dependency). `None` when the server
/// has not published a readable settings document.
fn native_settings_root(state: &AppState, kind: BackendKind) -> Option<Value> {
    let host_id = state.selected_host_id.get_untracked()?;
    state
        .backend_native_settings
        .get_untracked()
        .get(&host_id)
        .and_then(|m| m.get(&kind))
        .and_then(|snapshot| snapshot.settings.clone())
}

/// Set `value` at `path`/`key` inside `root`, creating intermediate objects as
/// needed so an edit always lands somewhere well-formed.
fn set_native_value(root: &mut Value, path: &[String], key: &str, value: Value) {
    let mut cursor = root;
    for segment in path {
        if !cursor.is_object() {
            *cursor = Value::Object(Map::new());
        }
        cursor = cursor
            .as_object_mut()
            .expect("cursor forced to object above")
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cursor.is_object() {
        *cursor = Value::Object(Map::new());
    }
    cursor
        .as_object_mut()
        .expect("cursor forced to object above")
        .insert(key.to_owned(), value);
}

/// Apply one native-settings edit and send the whole updated document to the
/// server via `HostSettingValue::BackendNativeSettings`. The backend replaces its
/// native settings document wholesale (Tycode `SaveSettings { persist: true }`),
/// so the full object is sent rather than a partial patch.
///
/// With `profile: Some(name)` the document is Tycode's profiles document: the
/// edit lands inside that profile's `settings` and the unedited settings are
/// attached as `base_settings`, so the server refuses a stale save instead of
/// overwriting concurrent changes to the real file.
fn commit_native_setting(
    state: &AppState,
    kind: BackendKind,
    profile: Option<&str>,
    path: &[String],
    key: &str,
    value: Value,
) {
    let Some(base) = native_settings_root(state, kind) else {
        log::error!(
            "cannot edit backend-native settings for {kind:?}: no current settings document"
        );
        return;
    };
    let mut root = base.clone();
    match profile {
        None => {
            set_native_value(&mut root, path, key, value);
            // No-op: the edit didn't change the document. Don't send or lock —
            // a save that leaves the document unchanged is pointless, and
            // locking on it risks stranding the page in "Saving…".
            if root == base {
                return;
            }
        }
        Some(name) => {
            let Some(entry) = root
                .get_mut("profiles")
                .and_then(Value::as_array_mut)
                .and_then(|profiles| {
                    profiles
                        .iter_mut()
                        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
                })
                .and_then(Value::as_object_mut)
            else {
                log::error!(
                    "cannot edit {kind:?} settings: profile '{name}' is not in the current document"
                );
                return;
            };
            let Some(original) = entry.get("settings").cloned() else {
                log::error!("cannot edit {kind:?} settings: profile '{name}' has no settings");
                return;
            };
            let mut edited = original.clone();
            set_native_value(&mut edited, path, key, value);
            if edited == original {
                return;
            }
            entry.insert("settings".to_owned(), edited);
            entry.insert("base_settings".to_owned(), original);
        }
    }
    send_native_settings_document(state, kind, base, root);
}

/// Send a full native-settings document to the server and record the save as
/// `Pending` against the pre-edit `base` document.
///
/// A native save is a full-document replace, so a second edit based on the same
/// (now stale) snapshot would clobber the first. The UI disables native
/// controls until the server force-emits a fresh native-settings snapshot
/// (which it does after every native save, even an unchanged one — see the
/// `BackendConfigSnapshots` dispatch handler that clears the pending gate). On
/// send failure the state flips to `Failed` so the controls re-enable and the
/// error surfaces. Values are never logged.
fn send_native_settings_document(state: &AppState, kind: BackendKind, base: Value, root: Value) {
    let Some((host_id, host_stream)) = state.selected_host_stream_untracked() else {
        log::error!("cannot save backend-native settings for {kind:?}: no selected host stream");
        return;
    };
    // Guard the wire path: if a save against this same base is already in
    // flight, drop the edit (the controls are disabled, but synthetic events
    // could still reach here).
    let already_pending = state
        .native_settings_save_state
        .get_untracked()
        .get(&host_id)
        .and_then(|m| m.get(&kind))
        .is_some_and(
            |save| matches!(save, NativeSettingsSaveState::Pending { base: b } if *b == base),
        );
    if already_pending {
        return;
    }

    state.native_settings_save_state.update(|states| {
        states
            .entry(host_id.clone())
            .or_default()
            .insert(kind, NativeSettingsSaveState::Pending { base });
    });

    let state = state.clone();
    let host_for_error = host_id.clone();
    spawn_local(async move {
        let payload = SetSettingPayload {
            setting: HostSettingValue::BackendNativeSettings {
                backend: kind,
                settings: root,
            },
        };
        if let Err(error) = send_frame(&host_id, host_stream, FrameKind::SetSetting, &payload).await
        {
            log::error!("failed to send BackendNativeSettings for {kind:?}: {error}");
            state.native_settings_save_state.update(|states| {
                states.entry(host_for_error).or_default().insert(
                    kind,
                    NativeSettingsSaveState::Failed {
                        message: "Failed to save settings. Check the connection and try again."
                            .to_owned(),
                    },
                );
            });
        }
    });
}

/// Send Tycode profile file operations (create/delete) as a profiles-document
/// save carrying `actions`; per-profile settings are echoed unchanged so the
/// server skips them.
fn send_tycode_profile_actions(state: &AppState, actions: Value) {
    let kind = BackendKind::Tycode;
    let Some(base) = native_settings_root(state, kind) else {
        log::error!("cannot modify Tycode profiles: no current settings document");
        return;
    };
    let mut root = base.clone();
    let Some(object) = root.as_object_mut() else {
        log::error!("cannot modify Tycode profiles: settings document is not an object");
        return;
    };
    object.insert("actions".to_owned(), actions);
    send_native_settings_document(state, kind, base, root);
}

/// One typed, server-classified advisory.
///
/// A `Ready` snapshot stays Ready **and editable** while these are present: the
/// advisory is the diagnosis and editing the settings below is the remedy, so
/// none of these disables a control. Each variant is matched by type; the
/// server's message is rendered verbatim and is never parsed to decide what to
/// show.
fn native_advisory_view(kind: BackendKind, advisory: &BackendNativeSettingsAdvisory) -> AnyView {
    let backend = backend_label(kind);
    match advisory {
        BackendNativeSettingsAdvisory::NoProviderConfigured { message } => view! {
            <div
                class="settings-native-advisory settings-native-advisory-no-provider"
                role="status"
            >
                <p class="settings-native-advisory-title">"No provider configured"</p>
                <p class="settings-native-advisory-text">{message.clone()}</p>
            </div>
        }
        .into_any(),
        BackendNativeSettingsAdvisory::BackendReported { message } => view! {
            <div class="settings-native-advisory settings-native-advisory-backend" role="status">
                <p class="settings-native-advisory-title">{format!("{backend} reported")}</p>
                <p class="settings-native-advisory-text">{message.clone()}</p>
            </div>
        }
        .into_any(),
    }
}

/// Typed, server-owned disclosures for a Ready native-settings snapshot: every
/// advisory the server published with it.
///
/// All of it is a projection of `advisories` alone. A snapshot carrying none
/// renders nothing here.
fn native_settings_disclosures(
    kind: BackendKind,
    snapshot: &BackendNativeSettingsSnapshot,
) -> Option<AnyView> {
    let advisories = snapshot
        .advisories
        .iter()
        .map(|advisory| native_advisory_view(kind, advisory))
        .collect::<Vec<_>>();

    if advisories.is_empty() {
        return None;
    }

    Some(
        view! {
            <div class="settings-native-disclosures">
                {advisories}
            </div>
        }
        .into_any(),
    )
}

/// One backend's native settings page body. Explicit unavailable/ready states —
/// current values never render as blank/default before the server publishes
/// them, and an unavailable snapshot shows the server's own reason verbatim.
fn backend_native_settings_body(
    state: &AppState,
    kind: BackendKind,
    snapshot: &BackendNativeSettingsSnapshot,
    active_native_group: RwSignal<Option<String>>,
    active_native_profile: RwSignal<Option<String>>,
) -> AnyView {
    match snapshot.status {
        BackendConfigSnapshotStatus::Unavailable => {
            let message = snapshot.message.clone().unwrap_or_else(|| {
                format!(
                    "{}'s native settings are unavailable on the selected host.",
                    backend_label(kind)
                )
            });
            return view! {
                <div class="settings-native-unavailable">
                    <p class="settings-native-unavailable-text">{message}</p>
                </div>
            }
            .into_any();
        }
        BackendConfigSnapshotStatus::Ready => {}
    }

    // Server-owned disclosures. `Ready` is authoritative and editable even when
    // advisories are present, so these never gate the controls below — the
    // read-only state comes from `status: Unavailable` above and from nothing
    // else. Never inferred from the message text or the settings document.
    let disclosures = native_settings_disclosures(kind, snapshot);

    let Some(settings) = snapshot.settings.clone() else {
        // Ready but no document — never fabricate defaults; say so explicitly.
        return view! {
            <div class="settings-native-settings">
                {disclosures}
                <p class="settings-description">
                    "This backend reported its native settings are ready but sent no current values."
                </p>
            </div>
        }
        .into_any();
    };

    if snapshot.groups.is_empty() {
        return view! {
            <div class="settings-native-settings">
                {disclosures}
                <p class="settings-description">
                    "This backend exposes native settings but no editable groups."
                </p>
            </div>
        }
        .into_any();
    }

    // A native save replaces the whole document, so while one is in flight the
    // controls are disabled until the server publishes a newer snapshot — a
    // second edit off the stale snapshot would clobber the first.
    let (saving, error) = native_save_indicator(state, kind, &settings);
    let saving_banner = saving.then(|| {
        view! {
            <div class="settings-native-saving" role="status">
                "Saving… settings are locked until the backend confirms the change."
            </div>
        }
    });
    let error_banner = error.map(|message| {
        view! {
            <div class="settings-native-error" role="alert">
                {message}
            </div>
        }
    });

    // Tycode's document is a profiles document (one entry per settings file);
    // it renders as profile chips over the shared schema-driven form. Every
    // other backend's document is the settings object itself.
    let form = if kind == BackendKind::Tycode {
        tycode_profiles_form(
            state,
            snapshot,
            &settings,
            saving,
            active_native_group,
            active_native_profile,
        )
    } else {
        native_settings_groups_form(
            state,
            kind,
            &snapshot.groups,
            &settings,
            saving,
            active_native_group,
            None,
        )
    };

    view! {
        <div class="settings-native-settings">
            {disclosures}
            {error_banner}
            {saving_banner}
            {form}
        </div>
    }
    .into_any()
}

/// Where a native-settings edit lands: which backend's document, optionally
/// scoped to one Tycode profile inside it.
#[derive(Clone)]
struct NativeEditScope {
    kind: BackendKind,
    profile: Option<String>,
}

/// The grouped, schema-driven editor for one settings document — a backend's
/// whole document, or one Tycode profile's settings when `profile` is set. A
/// single group renders with its own header; multiple groups render as tabs.
fn native_settings_groups_form(
    state: &AppState,
    kind: BackendKind,
    groups: &[BackendNativeSettingsGroup],
    settings: &Value,
    saving: bool,
    active_native_group: RwSignal<Option<String>>,
    profile: Option<&str>,
) -> AnyView {
    // Order groups Core-first, then Modules, preserving the server's order
    // within each kind. Core is the anchor page; module groups sit beside it as
    // tabs so a big backend (e.g. Tycode with per-provider modules) never
    // renders as one long flat form.
    let mut ordered: Vec<&BackendNativeSettingsGroup> = groups
        .iter()
        .filter(|group| group.kind == BackendNativeSettingsGroupKind::Core)
        .collect();
    ordered.extend(
        groups
            .iter()
            .filter(|group| group.kind == BackendNativeSettingsGroupKind::Module),
    );

    let scope = NativeEditScope {
        kind,
        profile: profile.map(str::to_owned),
    };

    // A single group needs no tab strip — render it with its own header.
    if ordered.len() == 1 {
        return native_settings_group(state, &scope, ordered[0], settings, saving);
    }

    // The active tab is tracked by group id (not index) so the selection is
    // stable if the group set changes. The selection lives in the page-level
    // `active_native_group` signal so it survives this body being rebuilt on
    // save-state/snapshot changes. `None` (or a stale id no longer in the
    // group set) resolves to the first (Core) group. Only the active group's
    // fields are visible; the rest stay mounted-but-hidden so tab switches keep
    // their in-progress edits.
    let ordered_ids: Vec<String> = ordered.iter().map(|group| group.id.clone()).collect();
    let default_id = ordered_ids[0].clone();
    let effective_active = Signal::derive(move || {
        active_native_group
            .get()
            .filter(|id| ordered_ids.contains(id))
            .unwrap_or_else(|| default_id.clone())
    });

    let tabs = ordered
        .iter()
        .map(|group| {
            let id = group.id.clone();
            let is_active = {
                let id = id.clone();
                Signal::derive(move || effective_active.get() == id)
            };
            let on_click = {
                let id = id.clone();
                move |_| active_native_group.set(Some(id.clone()))
            };
            view! {
                <button
                    type="button"
                    role="tab"
                    class=move || {
                        if is_active.get() {
                            "settings-native-tab settings-native-tab-active"
                        } else {
                            "settings-native-tab"
                        }
                    }
                    aria-selected=move || is_active.get().to_string()
                    on:click=on_click
                >
                    <span class="settings-native-tab-label">{group.title.clone()}</span>
                    <span class="settings-native-tab-badge">
                        {native_group_badge(group.kind)}
                    </span>
                </button>
            }
        })
        .collect::<Vec<_>>();

    let panels = ordered
        .iter()
        .map(|group| {
            let id = group.id.clone();
            let hidden = move || effective_active.get() != id;
            let content = native_settings_group_content(state, &scope, group, settings, saving);
            view! {
                <div
                    class="settings-native-group settings-native-group-panel"
                    role="tabpanel"
                    hidden=hidden
                >
                    {content}
                </div>
            }
        })
        .collect::<Vec<_>>();

    view! {
        <div class="settings-native-form">
            <div class="settings-native-tabs" role="tablist">{tabs}</div>
            <div class="settings-native-panels">{panels}</div>
        </div>
    }
    .into_any()
}

/// Tycode's native settings arrive as a profiles document: one entry per real
/// settings file (`~/.tycode/settings.toml` plus `~/.tycode/profiles/*.toml`).
/// Profile chips select which file the shared schema-driven form edits; edits
/// are scoped to that profile and refused by the server when stale. Profiles
/// can be created (copying the selected profile's file) and deleted here. A
/// malformed document is a visible error, never a blank form.
fn tycode_profiles_form(
    state: &AppState,
    snapshot: &BackendNativeSettingsSnapshot,
    doc: &Value,
    saving: bool,
    active_native_group: RwSignal<Option<String>>,
    active_native_profile: RwSignal<Option<String>>,
) -> AnyView {
    let kind = BackendKind::Tycode;
    let Some(profiles) = doc.get("profiles").and_then(Value::as_array) else {
        return view! {
            <p class="settings-description">
                "Tycode published a settings document without a profiles list. Refresh the snapshot; if this persists, the host and client disagree on the settings format."
            </p>
        }
        .into_any();
    };
    let names: Vec<String> = profiles
        .iter()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let Some(default_name) = names.first().cloned() else {
        return view! {
            <p class="settings-description">"No Tycode settings profiles were discovered."</p>
        }
        .into_any();
    };
    let names_for_active = names.clone();
    let effective_profile = Signal::derive(move || {
        active_native_profile
            .get()
            .filter(|name| names_for_active.contains(name))
            .unwrap_or_else(|| default_name.clone())
    });

    let chips = names
        .iter()
        .map(|name| {
            let chip_name = name.clone();
            let is_active = {
                let chip = chip_name.clone();
                Signal::derive(move || effective_profile.get() == chip)
            };
            let on_click = {
                let chip = chip_name.clone();
                move |_| active_native_profile.set(Some(chip.clone()))
            };
            view! {
                <button
                    type="button"
                    role="tab"
                    class=move || {
                        if is_active.get() {
                            "settings-native-profile-chip settings-native-profile-chip-active"
                        } else {
                            "settings-native-profile-chip"
                        }
                    }
                    aria-selected=move || is_active.get().to_string()
                    on:click=on_click
                >
                    {chip_name.clone()}
                </button>
            }
        })
        .collect::<Vec<_>>();

    let create_control = tycode_profile_create_control(state, saving, effective_profile);

    // Only the selected profile's panel is rendered. Edits are committed on
    // change (there is no draft to lose), so a chip switch can rebuild the
    // form from the document without discarding anything.
    let panel = {
        let state = state.clone();
        let groups = snapshot.groups.clone();
        let profiles = profiles.clone();
        move || {
            let active = effective_profile.get();
            let Some(entry) = profiles
                .iter()
                .find(|entry| entry.get("name").and_then(Value::as_str) == Some(active.as_str()))
            else {
                return view! {
                    <p class="settings-description">
                        "The selected profile is no longer in the settings document."
                    </p>
                }
                .into_any();
            };
            let Some(settings) = entry.get("settings") else {
                return view! {
                    <p class="settings-description">
                        "This profile's settings are missing from the document."
                    </p>
                }
                .into_any();
            };
            let settings_path = entry
                .get("settings_path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let form = native_settings_groups_form(
                &state,
                kind,
                &groups,
                settings,
                saving,
                active_native_group,
                Some(&active),
            );
            let delete_button = (active != protocol::tycode_config::TYCODE_DEFAULT_PROFILE).then(
                || {
                    let state = state.clone();
                    let profile = active.clone();
                    let on_delete = move |_| {
                        if saving {
                            return;
                        }
                        let state = state.clone();
                        let profile = profile.clone();
                        spawn_local(async move {
                            let message = format!(
                                "Delete the Tycode profile '{profile}'? Its settings file is removed permanently."
                            );
                            if !crate::bridge::confirm_dialog("Delete Tycode profile", &message)
                                .await
                            {
                                return;
                            }
                            send_tycode_profile_actions(
                                &state,
                                serde_json::json!([{ "kind": "delete_profile", "name": profile }]),
                            );
                        });
                    };
                    view! {
                        <button
                            type="button"
                            class="settings-btn settings-btn-danger settings-native-profile-delete"
                            disabled=saving
                            on:click=on_delete
                        >
                            "Delete profile"
                        </button>
                    }
                },
            );
            view! {
                <div class="settings-native-profile-panel" role="tabpanel">
                    <div class="settings-native-profile-meta">
                        <span class="settings-native-profile-path">{settings_path}</span>
                        {delete_button}
                    </div>
                    {form}
                </div>
            }
            .into_any()
        }
    };

    view! {
        <div class="settings-native-profiles">
            <div
                class="settings-native-profile-chips"
                role="tablist"
                aria-label="Tycode settings profiles"
            >
                {chips}
                {create_control}
            </div>
            {panel}
        </div>
    }
    .into_any()
}

/// Inline create-profile control: a chip-styled toggle revealing a name input.
/// The new profile copies the currently selected profile's settings file; the
/// server enforces the profile-name grammar and refuses collisions.
fn tycode_profile_create_control(
    state: &AppState,
    saving: bool,
    effective_profile: Signal<String>,
) -> AnyView {
    let open = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let state = state.clone();
    let on_create = move |_| {
        if saving {
            return;
        }
        let new_name = name.get_untracked().trim().to_owned();
        if new_name.is_empty() {
            return;
        }
        send_tycode_profile_actions(
            &state,
            serde_json::json!([{
                "kind": "create_profile",
                "name": new_name,
                "copy_from": effective_profile.get_untracked(),
            }]),
        );
        open.set(false);
        name.set(String::new());
    };
    view! {
        <div class="settings-native-profile-create">
            <button
                type="button"
                class="settings-native-profile-chip settings-native-profile-chip-new"
                disabled=saving
                on:click=move |_| open.update(|value| *value = !*value)
            >
                "+ New profile"
            </button>
            <div class="settings-native-profile-create-form" hidden=move || !open.get()>
                <input
                    type="text"
                    class="settings-input settings-native-profile-name"
                    placeholder="profile-name (a-z, 0-9, -, _)"
                    prop:value=move || name.get()
                    on:input=move |ev| {
                        let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                        name.set(el.value());
                    }
                    disabled=saving
                />
                <button
                    type="button"
                    class="settings-btn settings-btn-primary"
                    disabled=saving
                    on:click=on_create
                >
                    "Create"
                </button>
            </div>
        </div>
    }
    .into_any()
}

/// Short badge text distinguishing a Core group from a Module group. Shared by
/// the single-group header and the multi-group tab strip.
fn native_group_badge(kind: BackendNativeSettingsGroupKind) -> &'static str {
    match kind {
        BackendNativeSettingsGroupKind::Core => "Core",
        BackendNativeSettingsGroupKind::Module => "Module",
    }
}

/// The description + editable fields for one native-settings group, without any
/// title header. Reused by both the single-group section (which adds its own
/// header) and the tabbed multi-group panels (whose header lives in the tab).
fn native_settings_group_content(
    state: &AppState,
    scope: &NativeEditScope,
    group: &BackendNativeSettingsGroup,
    settings: &Value,
    disabled: bool,
) -> AnyView {
    let group_value = native_value_at_path(settings, &group.settings_path);
    let description = group
        .description
        .clone()
        .map(|text| view! { <p class="settings-native-group-desc">{text}</p> });

    let body = match group.schema.get("properties").and_then(Value::as_object) {
        Some(properties) => {
            // Distinguish "this group's path is absent (or explicit null) in the
            // document" from "the path is present but a property is unset" —
            // neither may render as a blank/default control that looks like a
            // real current value.
            let path_present = group_value.is_some_and(|value| !value.is_null());
            let empty = Map::new();
            let obj = group_value.and_then(Value::as_object).unwrap_or(&empty);
            let missing_note = (!path_present).then(|| {
                view! {
                    <p class="settings-native-unset-note">
                        "These settings are not present in the current document. Fields below are unset until you set a value."
                    </p>
                }
            });
            let fields = properties
                .iter()
                .map(|(key, prop_schema)| {
                    native_settings_field(
                        state,
                        scope,
                        &group.settings_path,
                        key,
                        prop_schema,
                        obj.get(key),
                        disabled,
                    )
                })
                .collect::<Vec<_>>();
            view! {
                {missing_note}
                <div class="settings-native-fields">{fields}</div>
            }
            .into_any()
        }
        None => {
            // No property map — don't drop the group. Render its whole value as a
            // read-only JSON view with secrets recursively redacted so nothing is
            // silently hidden and no secret leaks. An absent (or explicit-null)
            // path is stated explicitly rather than shown as a bare `null`.
            match group_value.filter(|value| !value.is_null()) {
                None => view! {
                    <p class="settings-native-unset-note">
                        "These settings are not present in the current document."
                    </p>
                }
                .into_any(),
                Some(value) => {
                    let json = pretty_json(&redact_secrets(value));
                    view! { <pre class="settings-native-json-readonly">{json}</pre> }.into_any()
                }
            }
        }
    };

    view! {
        {description}
        {body}
    }
    .into_any()
}

/// A single native-settings group rendered with its own titled header. Used when
/// a backend exposes exactly one group, where a tab strip would be noise.
fn native_settings_group(
    state: &AppState,
    scope: &NativeEditScope,
    group: &BackendNativeSettingsGroup,
    settings: &Value,
    disabled: bool,
) -> AnyView {
    let content = native_settings_group_content(state, scope, group, settings, disabled);
    view! {
        <section class="settings-native-group">
            <div class="settings-native-group-header">
                <span class="settings-native-group-title">{group.title.clone()}</span>
                <span class="settings-native-group-badge">
                    {native_group_badge(group.kind)}
                </span>
            </div>
            {content}
        </section>
    }
    .into_any()
}

/// One editable native settings field, generated from a JSON-schema property.
/// Renders a typed control for primitives/enums, masks secret keys, and falls
/// back to a visible JSON editor for object/array/unknown shapes so no field is
/// dropped.
fn native_settings_field(
    state: &AppState,
    scope: &NativeEditScope,
    path: &[String],
    key: &str,
    prop_schema: &Value,
    current: Option<&Value>,
    disabled: bool,
) -> AnyView {
    let label = prop_schema
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_owned();
    let description = prop_schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let secret = is_secret_native_key(key, prop_schema);
    // Nullable type arrays (e.g. `["string", "null"]`) still resolve to a typed
    // control rather than falling through to raw JSON editing.
    let schema_type = native_primitive_type(prop_schema);
    let schema_type = schema_type.as_deref();
    let enum_values: Vec<String> = prop_schema
        .get("enum")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    // A field is "configured" only when the server actually holds a value for
    // it. An absent key and an explicit JSON `null` are both unset — surfaced
    // explicitly so a blank/unchecked control is never mistaken for a real
    // current value. Controls stay editable either way so the user can set one.
    let present = current.is_some_and(|value| !value.is_null());

    let path = path.to_vec();
    let key = key.to_owned();
    let scope = scope.clone();

    let control = if secret {
        let has_value = current
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let placeholder = if has_value {
            "•••••••• (stored — type to replace)".to_owned()
        } else if present {
            String::new()
        } else {
            "Not set".to_owned()
        };
        let state = state.clone();
        let on_change = move |ev: web_sys::Event| {
            if disabled {
                return;
            }
            let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
            commit_native_setting(
                &state,
                scope.kind,
                scope.profile.as_deref(),
                &path,
                &key,
                Value::String(el.value()),
            );
        };
        view! {
            <input
                type="password"
                class="settings-input settings-native-input"
                placeholder=placeholder
                autocomplete="off"
                disabled=disabled
                on:change=on_change
            />
        }
        .into_any()
    } else if !enum_values.is_empty() {
        let current = current
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let option_views = enum_values
            .iter()
            .map(|value| view! { <option value=value.clone()>{value.clone()}</option> })
            .collect::<Vec<_>>();
        let state = state.clone();
        let on_change = move |ev: web_sys::Event| {
            if disabled {
                return;
            }
            let el: web_sys::HtmlSelectElement = ev.target().unwrap().unchecked_into();
            commit_native_setting(
                &state,
                scope.kind,
                scope.profile.as_deref(),
                &path,
                &key,
                Value::String(el.value()),
            );
        };
        view! {
            <select
                class="settings-select"
                prop:value=current
                disabled=disabled
                on:change=on_change
            >
                {(!present).then(|| view! { <option value="">"Not set"</option> })}
                {option_views}
            </select>
        }
        .into_any()
    } else {
        match schema_type {
            Some("boolean") => {
                let current = current.and_then(Value::as_bool).unwrap_or(false);
                let state = state.clone();
                let on_change = move |ev: web_sys::Event| {
                    if disabled {
                        return;
                    }
                    let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                    commit_native_setting(
                        &state,
                        scope.kind,
                        scope.profile.as_deref(),
                        &path,
                        &key,
                        Value::Bool(el.checked()),
                    );
                };
                view! {
                    <label class="settings-toggle">
                        <input
                            type="checkbox"
                            prop:checked=current
                            disabled=disabled
                            on:change=on_change
                        />
                        <span class="settings-toggle-slider"></span>
                    </label>
                }
                .into_any()
            }
            Some("integer") => {
                let current = current.and_then(Value::as_i64);
                let state = state.clone();
                let on_change = move |ev: web_sys::Event| {
                    if disabled {
                        return;
                    }
                    let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                    if let Ok(parsed) = el.value().parse::<i64>() {
                        commit_native_setting(
                            &state,
                            scope.kind,
                            scope.profile.as_deref(),
                            &path,
                            &key,
                            Value::from(parsed),
                        );
                    }
                };
                view! {
                    <input
                        type="number"
                        step="1"
                        class="settings-input settings-native-input"
                        prop:value=move || current.map(|n| n.to_string()).unwrap_or_default()
                        placeholder=(!present).then(|| "Not set".to_owned())
                        autocomplete="off"
                        disabled=disabled
                        on:change=on_change
                    />
                }
                .into_any()
            }
            Some("number") => {
                let current = current.and_then(Value::as_f64);
                let state = state.clone();
                let on_change = move |ev: web_sys::Event| {
                    if disabled {
                        return;
                    }
                    let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                    if let Ok(parsed) = el.value().parse::<f64>()
                        && let Some(number) = serde_json::Number::from_f64(parsed)
                    {
                        commit_native_setting(
                            &state,
                            scope.kind,
                            scope.profile.as_deref(),
                            &path,
                            &key,
                            Value::Number(number),
                        );
                    }
                };
                view! {
                    <input
                        type="number"
                        step="any"
                        class="settings-input settings-native-input"
                        prop:value=move || current.map(|n| n.to_string()).unwrap_or_default()
                        placeholder=(!present).then(|| "Not set".to_owned())
                        autocomplete="off"
                        disabled=disabled
                        on:change=on_change
                    />
                }
                .into_any()
            }
            Some("string") => {
                let current = current
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let state = state.clone();
                let on_change = move |ev: web_sys::Event| {
                    if disabled {
                        return;
                    }
                    let el: web_sys::HtmlInputElement = ev.target().unwrap().unchecked_into();
                    commit_native_setting(
                        &state,
                        scope.kind,
                        scope.profile.as_deref(),
                        &path,
                        &key,
                        Value::String(el.value()),
                    );
                };
                view! {
                    <input
                        type="text"
                        class="settings-input settings-native-input"
                        prop:value=current
                        placeholder=(!present).then(|| "Not set".to_owned())
                        autocomplete="off"
                        spellcheck="false"
                        disabled=disabled
                        on:change=on_change
                    />
                }
                .into_any()
            }
            _ => native_json_field_control(state, scope, path, key, current, disabled),
        }
    };

    // Explicit unset marker so a blank control is never read as a real value.
    let unset_caption =
        (!present).then(|| view! { <span class="settings-native-unset">"Unset"</span> });

    view! {
        <div class="settings-native-field">
            <span class="settings-tier-select-label">{label}</span>
            {control}
            {unset_caption}
            {description.map(|text| view! { <p class="settings-description">{text}</p> })}
        </div>
    }
    .into_any()
}

/// Conservative editor/view for object/array/unknown native settings.
///
/// - Absent value → an explicit "not set" JSON editor seeded empty (never a
///   `null` that looks like a real value).
/// - Value containing a secret at any depth → a read-only, recursively redacted
///   JSON view. Editing is refused because saving the redacted document would
///   clobber the real secret.
/// - Otherwise → a JSON textarea that commits on valid parse and surfaces a
///   parse error inline rather than silently discarding the edit.
///
/// The editor is also disabled while a native save is in flight.
fn native_json_field_control(
    state: &AppState,
    scope: NativeEditScope,
    path: Vec<String>,
    key: String,
    current: Option<&Value>,
    disabled: bool,
) -> AnyView {
    if let Some(value) = current
        && contains_secret(value)
    {
        let json = pretty_json(&redact_secrets(value));
        return view! {
            <div class="settings-native-json">
                <pre class="settings-native-json-readonly">{json}</pre>
                <p class="settings-native-json-note">
                    "Contains secret values and can't be edited here — change it through the backend directly so the stored secret isn't overwritten."
                </p>
            </div>
        }
        .into_any();
    }

    // An absent value and an explicit JSON `null` are both unset: show an empty
    // editor with a "not set" hint rather than a literal `null` that reads as a
    // real value.
    let present = current.is_some_and(|value| !value.is_null());
    let initial = current
        .filter(|value| !value.is_null())
        .map(pretty_json)
        .unwrap_or_default();
    let error = RwSignal::new(Option::<String>::None);
    let state = state.clone();
    let on_change = move |ev: web_sys::Event| {
        if disabled {
            return;
        }
        let el: web_sys::HtmlTextAreaElement = ev.target().unwrap().unchecked_into();
        let raw = el.value();
        if raw.trim().is_empty() {
            error.set(Some("Enter JSON to set a value.".to_owned()));
            return;
        }
        match serde_json::from_str::<Value>(&raw) {
            Ok(parsed) => {
                error.set(None);
                commit_native_setting(
                    &state,
                    scope.kind,
                    scope.profile.as_deref(),
                    &path,
                    &key,
                    parsed,
                );
            }
            Err(err) => error.set(Some(format!("Invalid JSON: {err}"))),
        }
    };
    view! {
        <div class="settings-native-json">
            <textarea
                class="settings-input settings-native-json-input"
                prop:value=initial
                placeholder=(!present).then(|| "Not set — enter JSON to set a value".to_owned())
                spellcheck="false"
                disabled=disabled
                on:change=on_change
            ></textarea>
            {move || {
                error
                    .get()
                    .map(|message| view! { <p class="settings-native-json-error">{message}</p> })
            }}
        </div>
    }
    .into_any()
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

/// Mobile pairing settings for `tycode.dev`-managed AWS IoT access.
/// Two host-scoped settings live here:
///   * `enable_mobile_connections` — master kill switch.
///   * `mobile_broker_url` — **dev/test-only** broker override. Production
///     mobile access is provisioned through `tycode.dev` managed pairing
///     onto AWS IoT Core; the server only honours this override for a
///     loopback broker in local development and fails closed for public /
///     free / custom production brokers. Empty input (the default) means
///     "use managed access" (None on the wire).
///
/// All mobile-access behaviour is server-owned: the frontend renders the
/// typed `MobileAccessStatePayload` (`broker_status` / `pairing`) and never
/// infers broker semantics or chooses a broker itself. Starting a pairing
/// initiates a server-owned managed pairing; the server decides managed vs.
/// the explicit loopback dev override, so the UI can never trigger an
/// unmanaged/public-broker fallback.
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

    // Start-pairing enablement is a pure function of typed server state:
    // enable Start whenever mobile is enabled and no pairing is already in
    // flight. It does NOT require the broker to be `Online` — in the managed
    // flow the broker only reaches `Online` *after* a pairing exists, so a
    // `Connecting` / `RepairRequired` (no pairing yet, or a stored pairing that
    // needs re-pairing) / `Error` broker status is exactly when the user needs
    // to start a fresh managed pairing. Starting is server-owned and cannot pick
    // an unmanaged/public broker, so gating it on broker status would only block
    // the (re-)pairing that resolves those states.
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
        let in_flight = mobile_start_pending()
            || matches!(pairing_phase(), Some(MobilePairingState::Active { .. }));
        !in_flight
    };

    view! {
        <h2 class="settings-panel-title">"Mobile"</h2>

        <p class="settings-description settings-panel-intro">
            "Pair the Tyde mobile app with this host over tycode.dev managed access. Pairing provisions a scoped, tycode.dev-signed AWS IoT broker connection — there is no public or free MQTT broker. Your mobile device signs in with a Tyggs Pass to complete pairing; this host is never asked for Tyggs credentials."
        </p>

        <div class="settings-field">
            <div class="settings-toggle-row">
                <div>
                    <label class="settings-label">"Enable mobile connections"</label>
                    <p class="settings-description">
                        "When enabled, this host can accept pairing requests from the Tyde mobile app and connect through tycode.dev-managed AWS IoT access."
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
                "Start a pairing session, then scan the QR code with the Tyde mobile app. The QR carries a one-time managed pairing offer, the managed broker endpoint, and a one-shot pre-shared key; the mobile app redeems the offer with tycode.dev to obtain scoped AWS IoT credentials. The pairing session expires after a couple of minutes."
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
                    "Start a fresh managed pairing session"
                } else if mobile_start_pending() {
                    "Starting pairing…"
                } else if matches!(pairing_phase(), Some(MobilePairingState::Active { .. })) {
                    "A pairing session is already active — cancel it first"
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
                                let (state_label, state_slug) = match device.state {
                                    MobileDeviceState::Connected => ("connected", "connected"),
                                    MobileDeviceState::Paired => ("offline", "offline"),
                                    MobileDeviceState::Revoked => ("revoked", "revoked"),
                                    MobileDeviceState::RepairRequired => {
                                        ("repair required", "repair-required")
                                    }
                                };
                                let state_class = format!("settings-mobile-pairing-device-state settings-mobile-pairing-device-state-{state_slug}");
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
            <label class="settings-label">"Broker URL (dev override)"</label>
            <p class="settings-description">
                "Advanced, local-development only. Production mobile access uses tycode.dev-managed AWS IoT, and the server fails closed for public, free, or custom production brokers. This override is honoured only for a loopback broker (localhost / 127.0.0.1) during local development. Leave blank for managed access."
            </p>
            <div class="settings-mobile-broker-row">
                <input
                    type="text"
                    class="settings-input settings-mobile-broker-input"
                    prop:value=broker_value
                    placeholder="wss://127.0.0.1:8083/mqtt"
                    disabled=broker_disabled_for_input
                    autocapitalize="none"
                    autocomplete="off"
                    spellcheck="false"
                    aria-label="Broker URL (dev override)"
                    aria-invalid=move || if broker_error.get().is_some() { "true" } else { "false" }
                    on:input=on_broker_input
                    on:change=on_broker_commit
                    on:keydown=on_broker_keydown
                />
                <button
                    type="button"
                    class="filter-toggle settings-mobile-broker-reset"
                    disabled=broker_disabled_for_button
                    title="Clear the dev override and use managed access"
                    on:click=on_broker_reset
                >
                    "Use managed"
                </button>
            </div>
            {move || broker_error.get().map(|message| view! {
                <p class="settings-mobile-broker-error" role="alert">{message}</p>
            })}
        </div>

        <div class="settings-mobile-warning" role="note">
            <p class="settings-mobile-warning-heading">
                "Managed access — encrypted contents, visible metadata"
            </p>
            <p class="settings-description">
                "Tyde end-to-end encrypts every message between this host and your paired mobile devices, so neither tycode.dev nor AWS IoT can read your chats, files, or commands. AWS IoT still sees connection metadata — client id, topic names, connection timing, and message sizes. tycode.dev mints short-lived, scoped broker credentials and never receives your Tyggs tokens or Tyde message contents."
            </p>
        </div>
    }
}

#[component]
fn BackendCard(kind: BackendKind, active_page: RwSignal<SettingsPage>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let name = backend_label(kind);
    let description = backend_description(kind);
    let badge_class = backend_badge_class(kind);
    let state_for_checked = state.clone();
    let state_for_disable = state.clone();
    let state_for_setup = state.clone();
    let state_for_configure = state.clone();

    // A card links to its settings page only when the server's schema catalog
    // says the backend is configurable — never hardcoded per backend.
    let has_settings_page = move || schema_backends(&state_for_configure).contains(&kind);

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
                    // An Unavailable backend (found but unusable) gets the same
                    // install command as a repair action, matching the server's
                    // "re-run the installer" diagnostics.
                    let install_label = match info.status {
                        BackendSetupStatus::NotInstalled => Some("Install"),
                        BackendSetupStatus::Unavailable => Some("Repair install"),
                        BackendSetupStatus::Installed | BackendSetupStatus::Unsupported => None,
                    }
                    .filter(|_| info.install_command.is_some());
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
                                {install_label.map(|label| view! {
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
                                        {label}
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

            {move || has_settings_page().then(|| view! {
                <div class="settings-backend-card-footer">
                    <button
                        class="settings-btn settings-backend-configure-btn"
                        on:click=move |_| active_page.set(SettingsPage::Backend(kind))
                    >
                        {format!("Configure {name}")}
                    </button>
                </div>
            })}

            <BackendSubscriptionCapacity kind />
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

/// The shared confirmation for every destructive settings flow: deleting a launch
/// profile, a custom agent, an MCP server, or a steering entry, and resetting
/// Tyde's managed projection.
///
/// All five are irreversible, so this is a real modal dialog rather than a styled
/// `<div>`. It announces itself (`role="alertdialog"`, `aria-modal`, and its
/// title and warning wired up as the accessible name and description), it takes
/// focus — onto **Cancel**, never onto the destructive button — it traps Tab
/// between its two controls, Escape cancels, and focus returns to whatever opened
/// it when it closes.
///
/// None of that used to be true. The keydown handler sat on the overlay and
/// nothing ever focused the overlay, so Escape was dead; focus stayed on the
/// button behind the modal, so Tab walked straight into the page underneath. For
/// an irreversible action the combination is sharp: with no announcement and no
/// trap, a keyboard user could tab blindly onto "Reset Tyde's copy" — a
/// `settings-btn-danger` — without ever being told a confirmation had opened.
///
/// The mouse flows are unchanged: the backdrop and Cancel both cancel.
#[component]
fn SettingsConfirmDialog(
    title: String,
    body: String,
    confirm_label: String,
    on_cancel: Callback<()>,
    on_confirm: Callback<()>,
) -> impl IntoView {
    // The accessible name and description are wired by id, so they have to be
    // unique per dialog instance. Each id is bound twice up front — once for the
    // element that owns it, once for the reference on the dialog — because the
    // `view!` macro builds children before it applies the parent's attributes.
    let title_id = generate_id();
    let body_id = generate_id();
    let labelled_by = title_id.clone();
    let described_by = body_id.clone();

    let cancel_ref = NodeRef::<leptos::html::Button>::new();
    let confirm_ref = NodeRef::<leptos::html::Button>::new();

    // Whatever holds focus right now — captured here, at construction, while it is
    // still the control the user activated to open this dialog. Capturing inside
    // the mount effect below would be too late: focus has moved to Cancel by then,
    // and a re-run would record Cancel as its own opener. `web_sys` elements are
    // not `Send`, hence the local storage.
    let opener: StoredValue<Option<web_sys::HtmlElement>, LocalStorage> = StoredValue::new_local(
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element())
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok()),
    );

    Effect::new(move |_| {
        if let Some(cancel) = cancel_ref.get() {
            // The safe landing. A destructive action is never the initial focus
            // target, so a keyboard user cannot confirm one by reflex.
            let _ = cancel.focus();
        }
    });

    // …and on close, focus goes back where it came from.
    on_cleanup(move || {
        if let Some(opener) = opener.get_value() {
            let _ = opener.focus();
        }
    });

    let cancel_on_backdrop = on_cancel;
    let on_backdrop = move |_| cancel_on_backdrop.run(());

    let cancel_on_keydown = on_cancel;
    let on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Escape" => {
            // A modal owns Escape outright. Without `stop_propagation` the event
            // carried on up to the app's global `window` keydown listener, whose
            // Escape arm closes the Settings overlay — so one Escape cancelled the
            // dialog *and* tore down the page behind it. The recovery card went
            // with it, and focus fell to `<body>`, because the opener this dialog
            // restores focus to had been unmounted along with the panel.
            ev.prevent_default();
            ev.stop_propagation();
            cancel_on_keydown.run(());
        }
        // The dialog has exactly two focusable controls, so the trap is a cycle
        // between them: Tab off the last wraps to the first, Shift+Tab off the
        // first wraps to the last. Anything else and Tab leaves a modal that is
        // covering the page.
        "Tab" => {
            let (Some(cancel), Some(confirm)) = (cancel_ref.get(), confirm_ref.get()) else {
                return;
            };
            let active = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.active_element());
            let focused = |button: &web_sys::HtmlButtonElement| {
                active
                    .as_ref()
                    .is_some_and(|el| el.is_same_node(Some(button.unchecked_ref())))
            };
            if ev.shift_key() {
                if focused(&cancel) {
                    ev.prevent_default();
                    let _ = confirm.focus();
                }
            } else if focused(&confirm) {
                ev.prevent_default();
                let _ = cancel.focus();
            }
        }
        _ => {}
    };

    let cancel_on_click = on_cancel;
    let on_cancel_click = move |_| cancel_on_click.run(());

    let confirm_on_click = on_confirm;
    let on_confirm_click = move |_| confirm_on_click.run(());

    view! {
        <div class="settings-confirm-overlay" on:click=on_backdrop>
            <div
                class="settings-confirm-modal settings-confirm-danger"
                role="alertdialog"
                aria-modal="true"
                aria-labelledby=labelled_by
                aria-describedby=described_by
                tabindex="-1"
                on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                on:keydown=on_keydown
            >
                <h3 class="settings-confirm-title" id=title_id>{title}</h3>
                <p class="settings-confirm-description" id=body_id>{body}</p>
                <div class="settings-form-footer">
                    <button
                        type="button"
                        class="settings-btn"
                        node_ref=cancel_ref
                        on:click=on_cancel_click
                    >
                        "Cancel"
                    </button>
                    <button
                        type="button"
                        class="settings-btn settings-btn-danger"
                        node_ref=confirm_ref
                        on:click=on_confirm_click
                    >
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
    // Only the tests construct command-error payloads, so this stays out of the
    // production import list — where it would be an unused import on any target that
    // compiles the crate without the wasm test module.
    use protocol::{CommandErrorCode, HostSettingErrorTarget, SelectOption};
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
                    supervisor: Default::default(),
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

    /// With no `mobile_broker_url` override the input renders empty. Its
    /// placeholder must advertise the dev-only loopback override — never a
    /// public / free broker (managed access is the production path).
    #[wasm_bindgen_test]
    async fn mobile_tab_broker_override_placeholder_is_loopback_not_public() {
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
        let placeholder = input.get_attribute("placeholder").unwrap_or_default();
        assert!(
            placeholder.contains("127.0.0.1") || placeholder.contains("localhost"),
            "broker override placeholder must be a loopback dev example; got {placeholder:?}"
        );
        assert!(
            !placeholder.contains("emqx") && !placeholder.to_lowercase().contains("public"),
            "broker override placeholder must not advertise a public/free broker; got {placeholder:?}"
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

    /// The Mobile tab copy must reflect **managed** tycode.dev / AWS IoT
    /// access: (1) it names managed access (tycode.dev / AWS IoT), (2) it
    /// mentions Tyde end-to-end encryption, (3) it calls out visible metadata,
    /// and (4) it must NOT frame the broker as a public / free / custom MQTT
    /// broker (that model no longer exists — the server fails closed for it).
    /// Tests user-perceived text, not CSS classes.
    #[wasm_bindgen_test]
    async fn mobile_tab_copy_reflects_managed_access_not_public_broker() {
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
            text.contains("tycode.dev") || text.contains("aws iot") || text.contains("managed"),
            "mobile copy must describe managed tycode.dev / AWS IoT access; got: {text:?}"
        );
        assert!(
            text.contains("encrypt"),
            "mobile copy must mention encryption; got: {text:?}"
        );
        assert!(
            text.contains("metadata"),
            "mobile copy must call out visible metadata; got: {text:?}"
        );
        // Inverse: no public/free/custom-broker framing, and Tyde is never the
        // broker operator.
        assert!(
            !text.contains("public mqtt broker")
                && !text.contains("public broker")
                && !text.contains("free public")
                && !text.contains("emqx"),
            "mobile copy must not present a public/free MQTT broker; got: {text:?}"
        );
        assert!(
            !text.contains("tyde broker"),
            "mobile copy must not say 'Tyde broker' (Tyde is the client); got: {text:?}"
        );
    }

    /// The "Use managed" button must always be present alongside the broker
    /// URL override input so the user can clear a dev override and return to
    /// tycode.dev-managed access without manually clearing the field.
    #[wasm_bindgen_test]
    async fn mobile_tab_has_use_managed_button() {
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
            if el.text_content().as_deref().map(str::trim) == Some("Use managed") {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "Mobile tab must surface a 'Use managed' button to clear the dev broker override"
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
        // The id must be unique per dispatch, and cleared after use. A fixed
        // id resolves `getElementById` to the FIRST tagged element in the
        // document, so a second dispatch in the same test (or a panel leaked
        // by an earlier panicked test — wasm panics skip destructors) would
        // silently re-fire a previous target instead of `input`. Observed as
        // one input's SetSetting frame recorded twice while the intended
        // target never fired.
        static DISPATCH_SEQ: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let seq = DISPATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("__tyde_dispatch_target_{kind}_{seq}");
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
                el.removeAttribute('id');
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

    /// Pressing Enter on a valid **loopback** override commits a `SetSetting`
    /// frame whose payload is `MobileBrokerUrl { broker_url: Some(...) }`.
    /// Load-bearing assertion that the typed-URL commit path actually reaches
    /// the wire for the only broker kind the server accepts.
    #[wasm_bindgen_test]
    async fn mobile_tab_enter_commits_valid_loopback_broker_url() {
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
        input.set_value("mqtts://127.0.0.1:8883");
        dispatch_enter(&input);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let mobile = settings
            .iter()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("mobile_broker_url"))
            .expect("Enter on a valid loopback broker URL must emit a MobileBrokerUrl frame");
        let broker_url = mobile
            .get("broker_url")
            .and_then(|v| v.as_str())
            .expect("MobileBrokerUrl payload must carry the URL on commit");
        assert_eq!(broker_url, "mqtts://127.0.0.1:8883");
    }

    /// Clicking "Use managed" commits `MobileBrokerUrl { broker_url: None }`.
    /// The server resolves None to tycode.dev-managed access, so this is how
    /// the user clears a dev override without manually clearing the field.
    #[wasm_bindgen_test]
    async fn mobile_tab_use_managed_button_commits_none() {
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
            if el.text_content().as_deref().map(str::trim) == Some("Use managed") {
                el.click();
                clicked = true;
                break;
            }
        }
        assert!(clicked, "Use managed button must be present and clickable");
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

    /// QA finding: a valid-scheme, valid-shape but **non-loopback** broker URL
    /// (which the server now rejects at write time) must fail closed inline —
    /// (a) no `MobileBrokerUrl` frame is sent, (b) the `settings-mobile-broker-error`
    /// message renders and names the loopback rule, and (c) the field flips
    /// `aria-invalid`. Previously this URL was sent and the rejection only
    /// appeared in the global header.
    #[wasm_bindgen_test]
    async fn mobile_tab_non_loopback_broker_shows_inline_error_and_suppresses_send() {
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
        input.set_value("mqtts://broker.example.test:8883");
        dispatch_enter(&input);
        for _ in 0..4 {
            next_tick().await;
        }

        // (a) Nothing is committed to the wire.
        let settings = recorded_set_setting_payloads(&calls);
        assert!(
            settings
                .iter()
                .all(|s| s.get("kind").and_then(|k| k.as_str()) != Some("mobile_broker_url")),
            "Non-loopback broker URL must not be committed; recorded settings: {settings:?}"
        );

        // (b) Inline error renders in the field's own error element and the copy
        // explains the loopback/managed rule so the user isn't left guessing.
        let error_el = container
            .query_selector(".settings-mobile-broker-error")
            .unwrap()
            .expect("Non-loopback broker URL must surface an inline error message");
        let error_text = error_el.text_content().unwrap_or_default().to_lowercase();
        assert!(
            error_text.contains("loopback"),
            "Inline error must explain the loopback rule; got: {error_text:?}"
        );
        assert!(
            error_text.contains("managed") || error_text.contains("localhost"),
            "Inline error should point the user at managed access / loopback; got: {error_text:?}"
        );

        // (c) aria-invalid announces the problem to assistive tech.
        assert_eq!(
            input.get_attribute("aria-invalid").as_deref(),
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

    /// When the managed broker is in Error state, the pairing card surfaces the
    /// server error message via the broker status pill AND keeps Start pairing
    /// enabled: in the managed flow, (re-)pairing is exactly how the user
    /// recovers from a broker error, so gating Start on broker health would only
    /// block the fix. (Starting is server-owned, so it can't pick an
    /// unmanaged/public broker.)
    #[wasm_bindgen_test]
    async fn mobile_tab_broker_error_keeps_start_enabled_and_shows_message() {
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
            !btn.has_attribute("disabled"),
            "Start pairing must stay enabled on broker error so the user can re-pair"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("broker unreachable"),
            "Broker error message must surface in the pairing card; got: {text:?}"
        );
    }

    /// First managed pairing: before any pairing exists the server reports
    /// `MobileBrokerStatus::RepairRequired` (there is no `Online` broker yet).
    /// Start pairing MUST be enabled in this state — otherwise the user can
    /// never start their first managed pairing — and the repair message must
    /// surface so the state is self-explanatory.
    #[wasm_bindgen_test]
    async fn mobile_tab_repair_required_enables_start_for_first_pairing() {
        let _calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_mobile_host_settings(&state, None, true);
            let payload = protocol::MobileAccessStatePayload {
                broker_status: MobileBrokerStatus::RepairRequired {
                    code: protocol::MobileAccessErrorCode::RepairRequired,
                    message:
                        "Mobile access requires a tycode.dev managed pairing before connecting"
                            .to_owned(),
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
            .expect("Start pairing button must render when a managed pairing is required");
        assert!(
            !btn.has_attribute("disabled"),
            "Start pairing must be enabled so the first managed pairing can begin"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("managed pairing"),
            "the repair-required message must surface; got: {text:?}"
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
                    supervisor: protocol::SupervisorSettings::default(),
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

    /// Toggling "Enable agent supervisor" on must commit a `SetSetting`
    /// frame whose payload is `SupervisorEnabled { enabled: true }`. Like the
    /// activity-summaries assertion, this is the load-bearing wire check for
    /// a paid opt-in feature.
    #[wasm_bindgen_test]
    async fn supervisor_tab_enable_toggle_commits_setting() {
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
        click_tab(&container, "Supervisor");
        next_tick().await;

        let toggle = toggle_for_label(&container, "Enable agent supervisor");
        assert!(
            !toggle.checked(),
            "the supervisor defaults off before the user opts in"
        );
        toggle.set_checked(true);
        dispatch_change(&toggle);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let frame = settings
            .iter()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("supervisor_enabled"))
            .expect("toggling the supervisor must emit a SupervisorEnabled SetSetting");
        assert_eq!(
            frame.get("enabled").and_then(|v| v.as_bool()),
            Some(true),
            "the committed frame must carry enabled=true: {frame:?}"
        );

        let compact = toggle_for_label(&container, "Auto-compact on success");
        compact.set_checked(true);
        dispatch_change(&compact);
        for _ in 0..4 {
            next_tick().await;
        }
        let settings = recorded_set_setting_payloads(&calls);
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("supervisor_auto_compact_on_success")
                    && s.get("enabled").and_then(|v| v.as_bool()) == Some(true)
            }),
            "toggling auto-compact must emit a SupervisorAutoCompactOnSuccess SetSetting: {settings:?}"
        );
    }

    /// The Supervisor tab number inputs must reflect the host defaults and
    /// commit typed count settings when edited.
    #[wasm_bindgen_test]
    async fn supervisor_tab_number_inputs_commit_counts() {
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
        click_tab(&container, "Supervisor");
        next_tick().await;

        let kicks: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Supervisor kick limit']")
            .unwrap()
            .expect("kick limit input renders")
            .dyn_into()
            .expect("kick limit is an input");

        let compact_min: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Supervisor auto-compact minimum context tokens']")
            .unwrap()
            .expect("auto-compact minimum context input renders")
            .dyn_into()
            .expect("auto-compact minimum context is an input");
        let compact_delay: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Supervisor auto-compact inactivity delay seconds']")
            .unwrap()
            .expect("auto-compact inactivity delay input renders")
            .dyn_into()
            .expect("auto-compact inactivity delay is an input");
        assert_eq!(
            compact_delay.value(),
            "300",
            "auto-compact inactivity delay shows the server default"
        );
        let compact_delay_unit = compact_delay
            .parent_element()
            .expect("delay input has a form row")
            .query_selector(".settings-supervisor-number-unit")
            .unwrap()
            .expect("auto-compact inactivity delay renders a visible unit");
        assert_eq!(
            compact_delay_unit.text_content().as_deref(),
            Some("seconds")
        );
        compact_delay.set_value("15");
        dispatch_change(&compact_delay);
        compact_delay.set_value("not-a-number");
        dispatch_change(&compact_delay);
        compact_delay.set_value("0");
        dispatch_change(&compact_delay);
        compact_delay.set_value("86401");
        dispatch_change(&compact_delay);
        assert_eq!(
            compact_min.value(),
            "200000",
            "auto-compact minimum context shows the host default before edits"
        );
        let compact_min_unit = compact_min
            .parent_element()
            .expect("minimum input has a form row")
            .query_selector(".settings-supervisor-number-unit")
            .unwrap()
            .expect("auto-compact minimum context renders a visible unit");
        assert_eq!(compact_min_unit.text_content().as_deref(), Some("tokens"));
        compact_min.set_value("240000");
        dispatch_change(&compact_min);
        compact_min.set_value("0");
        dispatch_change(&compact_min);

        assert_eq!(
            kicks.value(),
            "3",
            "kick limit shows the host default before edits"
        );
        kicks.set_value("5");
        dispatch_change(&kicks);

        let retries: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Supervisor extra delayed attempts']")
            .unwrap()
            .expect("retry attempts input renders")
            .dyn_into()
            .expect("retry attempts is an input");
        assert_eq!(
            retries.value(),
            "1",
            "retry attempts shows the host default before edits"
        );
        retries.set_value("2");
        dispatch_change(&retries);
        retries.set_value("6");
        dispatch_change(&retries);
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str())
                    == Some("supervisor_auto_compact_inactivity_delay_seconds")
                    && s.get("seconds").and_then(|v| v.as_u64()) == Some(15)
            }),
            "editing the delay must emit the exact typed setting: {settings:?}"
        );
        assert_eq!(
            settings
                .iter()
                .filter(|s| {
                    s.get("kind").and_then(|k| k.as_str())
                        == Some("supervisor_auto_compact_inactivity_delay_seconds")
                })
                .count(),
            1,
            "unparsable and out-of-range delay values must emit nothing: {settings:?}"
        );
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str())
                    == Some("supervisor_auto_compact_min_context_tokens")
                    && s.get("tokens").and_then(|v| v.as_u64()) == Some(240_000)
            }),
            "editing the auto-compact minimum must emit a typed nonzero setting: {settings:?}"
        );
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str())
                    == Some("supervisor_auto_compact_min_context_tokens")
                    && s.get("tokens").and_then(|v| v.as_u64()) == Some(0)
            }),
            "zero must be committed as an explicit auto-compact minimum: {settings:?}"
        );
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("supervisor_max_kicks_per_task")
                    && s.get("count").and_then(|v| v.as_u64()) == Some(5)
            }),
            "editing the kick limit must emit SupervisorMaxKicksPerTask count=5: {settings:?}"
        );
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("supervisor_retry_attempts")
                    && s.get("count").and_then(|v| v.as_u64()) == Some(2)
            }),
            "editing retry attempts must emit SupervisorRetryAttempts count=2: {settings:?}"
        );
        assert!(
            settings.iter().all(|s| {
                s.get("kind").and_then(|k| k.as_str()) != Some("supervisor_retry_attempts")
                    || s.get("count").and_then(|v| v.as_u64()) != Some(6)
            }),
            "out-of-range delayed attempts must emit nothing: {settings:?}"
        );

        let tier: web_sys::HtmlSelectElement = container
            .query_selector("select[aria-label='Supervisor verdict model tier']")
            .unwrap()
            .expect("verdict tier select renders")
            .dyn_into()
            .expect("verdict tier is a select");
        assert_eq!(
            tier.value(),
            "low",
            "the verdict tier defaults to the cheap tier"
        );
        tier.set_value("high");
        dispatch_event_from_js(&tier.clone().unchecked_into(), "change", None);
        for _ in 0..4 {
            next_tick().await;
        }
        let settings = recorded_set_setting_payloads(&calls);
        assert!(
            settings.iter().any(|s| {
                s.get("kind").and_then(|k| k.as_str()) == Some("supervisor_cost_tier")
                    && s.get("tier").and_then(|v| v.as_str()) == Some("high")
            }),
            "picking High must emit SupervisorCostTier tier=high: {settings:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn supervisor_auto_compact_controls_cascade_reactively() {
        let container = make_container();
        let state = AppState::new();
        install_general_host_settings(&state, true, false, None);
        state.settings_open.set(true);
        let mounted_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mounted_state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Supervisor");
        next_tick().await;

        let auto_compact = toggle_for_label(&container, "Auto-compact on success");
        let delay: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Supervisor auto-compact inactivity delay seconds']")
            .unwrap()
            .expect("delay input")
            .dyn_into()
            .expect("delay input element");
        let threshold: web_sys::HtmlInputElement = container
            .query_selector("input[aria-label='Supervisor auto-compact minimum context tokens']")
            .unwrap()
            .expect("threshold input")
            .dyn_into()
            .expect("threshold input element");
        assert!(auto_compact.disabled());
        assert!(delay.disabled());
        assert!(threshold.disabled());
        assert_eq!(delay.value(), "300");

        state.host_settings_by_host.update(|settings_by_host| {
            let settings = settings_by_host
                .get_mut("host-general")
                .expect("same selected host settings");
            settings.supervisor.enabled = true;
            settings.supervisor.auto_compact_inactivity_delay_seconds = 41;
        });
        next_tick().await;
        assert!(!auto_compact.disabled());
        assert!(delay.disabled());
        assert!(threshold.disabled());
        assert_eq!(delay.value(), "41", "disabled values remain server-owned");

        state.host_settings_by_host.update(|settings_by_host| {
            let settings = settings_by_host
                .get_mut("host-general")
                .expect("same selected host settings");
            settings.supervisor.auto_compact_on_success = true;
            settings.supervisor.auto_compact_inactivity_delay_seconds = 57;
        });
        next_tick().await;
        assert!(!delay.disabled());
        assert!(!threshold.disabled());
        assert_eq!(
            delay.value(),
            "57",
            "same-host same-key updates must replace the displayed delay"
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
        enabled_backends: Vec<BackendKind>,
    ) -> protocol::HostSettings {
        protocol::HostSettings {
            enabled_backends,
            default_backend: Some(BackendKind::Hermes),
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            supervisor: Default::default(),
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
            persistence_mode: BackendConfigPersistenceMode::TydeSettingsStore,
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

    /// A Tycode-shaped schema: a Select primary field plus a text field. Tycode
    /// is the backend whose `BackendConfig` edits persist natively right away,
    /// so it's the canonical fixture for the locked-page tests.
    fn tycode_config_schema() -> protocol::BackendConfigSchema {
        protocol::BackendConfigSchema {
            backend_kind: BackendKind::Tycode,
            persistence_mode: BackendConfigPersistenceMode::BackendNative,
            fields: vec![
                BackendConfigField {
                    key: "active_provider".to_owned(),
                    label: "Active Provider".to_owned(),
                    description: None,
                    field_type: BackendConfigFieldType::Select {
                        options: vec![
                            SelectOption {
                                value: "anthropic".to_owned(),
                                label: "Anthropic".to_owned(),
                            },
                            SelectOption {
                                value: "bedrock".to_owned(),
                                label: "Bedrock".to_owned(),
                            },
                        ],
                        default: Some("anthropic".to_owned()),
                        nullable: false,
                    },
                },
                BackendConfigField {
                    key: "profile".to_owned(),
                    label: "AWS Profile".to_owned(),
                    description: None,
                    field_type: BackendConfigFieldType::Text {
                        default: None,
                        placeholder: None,
                        multiline: false,
                    },
                },
            ],
        }
    }

    /// A backend settings page renders one control per schema field and seeds
    /// each control from the stored host-level value.
    #[wasm_bindgen_test]
    async fn backend_page_renders_schema_fields_and_seeds_stored_values() {
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
                host_settings_with_hermes_config(backend_config, vec![BackendKind::Hermes]),
            );
        });
        state.backend_config_schemas.update(|map| {
            map.entry(host_id.clone())
                .or_default()
                .insert(BackendKind::Hermes, hermes_config_schema());
        });
        state.backend_setup_by_host.update(|map| {
            map.insert(
                host_id.clone(),
                vec![backend_setup_info(
                    BackendKind::Hermes,
                    BackendSetupStatus::Installed,
                )],
            );
        });

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Hermes"),
            "page heading must name the backend: {text:?}"
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

    /// A backend page without a schema on the selected host renders an explicit
    /// empty state — never config inputs, never blank UI.
    ///
    /// Fixture correction (evidence, not a weakening): this test used Hermes
    /// as its schema-less backend, but Hermes no longer publishes a typed
    /// deep-config schema — its page now renders the backend-native settings
    /// experience, whose pre-snapshot render is the explicit "Waiting for
    /// Hermes settings from the selected host…" message (observed in the
    /// failing render). The generic no-schema contract is therefore pinned
    /// with Claude, and the Hermes pre-snapshot state is asserted explicitly
    /// on top — both surfaces must show a message, never blank UI.
    #[wasm_bindgen_test]
    async fn backend_page_without_schema_shows_explicit_empty_state() {
        let container = make_container();
        let state = AppState::new();
        let host_id = "host-a".to_owned();
        state.selected_host_id.set(Some(host_id.clone()));
        state.host_settings_by_host.update(|map| {
            map.insert(
                host_id.clone(),
                host_settings_with_hermes_config(
                    std::collections::HashMap::new(),
                    vec![BackendKind::Claude, BackendKind::Hermes],
                ),
            );
        });
        // No schema pushed for this host.

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Claude /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("No configuration is available"),
            "missing schema must render an explicit message, not blank UI: {text:?}"
        );
        assert_eq!(
            container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            0,
            "no config inputs without a schema"
        );

        // Hermes before its native-settings snapshot arrives: an explicit
        // waiting message, still no config inputs and never blank UI.
        let hermes_container = make_container();
        let state_for_hermes = state.clone();
        let _hermes_handle = mount_to(hermes_container.clone(), move || {
            provide_context(state_for_hermes.clone());
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
        });
        next_tick().await;

        let text = hermes_container.text_content().unwrap_or_default();
        assert!(
            text.contains("Waiting for Hermes settings"),
            "pre-snapshot Hermes page must render an explicit waiting message: {text:?}"
        );
        assert_eq!(
            hermes_container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            0,
            "no config inputs before the Hermes snapshot arrives"
        );
    }

    /// A server-shaped setup probe result for one backend.
    fn backend_setup_info(
        kind: BackendKind,
        status: BackendSetupStatus,
    ) -> protocol::BackendSetupInfo {
        protocol::BackendSetupInfo {
            backend_kind: kind,
            status,
            installed_version: None,
            docs_url: "https://example.test/docs".to_owned(),
            install_command: None,
            diagnostic: None,
            sign_in_command: None,
        }
    }

    /// Install a connected host with the Hermes deep-config schema plus stored
    /// values, and select it — enough for `BackendSettingsPage` to render and
    /// persist edits over the wire. Hermes is reported Installed so the page's
    /// availability lock stays open unless a test overrides it.
    fn install_backend_config_host(
        state: &AppState,
        values: BackendConfigValues,
        enabled_backends: Vec<BackendKind>,
    ) {
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
                host_settings_with_hermes_config(backend_config, enabled_backends),
            );
        });
        state.backend_config_schemas.update(|m| {
            m.entry(host_id.clone())
                .or_default()
                .insert(BackendKind::Hermes, hermes_config_schema());
        });
        state.backend_setup_by_host.update(|m| {
            m.insert(
                host_id,
                vec![backend_setup_info(
                    BackendKind::Hermes,
                    BackendSetupStatus::Installed,
                )],
            );
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
            install_backend_config_host(
                &state,
                stored_for_mount.clone(),
                vec![BackendKind::Hermes],
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
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
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            let mut native = BackendConfigValues::default();
            native.0.insert(
                "default_model".to_owned(),
                SessionSettingValue::String("anthropic/claude-opus".to_owned()),
            );
            set_backend_snapshot(&state, BackendConfigSnapshotStatus::Ready, native, None);
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
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
            install_backend_config_host(&state, overrides, vec![BackendKind::Hermes]);
            let mut native = BackendConfigValues::default();
            native.0.insert(
                "default_model".to_owned(),
                SessionSettingValue::String("native-model".to_owned()),
            );
            set_backend_snapshot(&state, BackendConfigSnapshotStatus::Ready, native, None);
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
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
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            set_backend_snapshot(
                &state,
                BackendConfigSnapshotStatus::Unavailable,
                BackendConfigValues::default(),
                Some("Hermes gateway not reachable"),
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
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

    // ---- Backends sidebar group + per-backend pages ----

    fn panel_title(container: &HtmlElement) -> String {
        container
            .query_selector(".settings-panel-title")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default()
    }

    /// The sidebar has a dedicated Backends group: a stable Overview entry plus
    /// one schema-derived item per configurable backend. The overview page no
    /// longer renders any backend config fields; the configurable backend's
    /// card links to its own settings page instead.
    #[wasm_bindgen_test]
    async fn backends_group_lists_schema_pages_and_overview_has_no_config_fields() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        let overview = find_button_by_text(&container, "Overview")
            .expect("the Backends group must have a stable Overview item");
        assert!(
            find_button_by_text(&container, "Hermes").is_some(),
            "a backend in the host's schema catalog must get its own nav item"
        );
        let task_complexity = find_button_by_text(&container, "Task Complexity")
            .expect("Task Complexity must have its own Backends nav item");
        assert!(
            find_button_by_text(&container, "Claude").is_none(),
            "a backend without a schema must not get a nav item"
        );

        overview.click();
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert_eq!(panel_title(&container), "Backends");
        assert!(
            text.contains("Default Backend"),
            "the overview keeps the global backend controls: {text:?}"
        );
        assert_eq!(
            container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            0,
            "backend config fields must no longer render on the overview"
        );
        assert!(
            !text.contains("Task complexity tiers"),
            "complexity controls must not remain on the Backends overview: {text:?}"
        );

        task_complexity.click();
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert_eq!(panel_title(&container), "Task Complexity");
        assert!(
            text.contains("Task complexity tiers"),
            "the dedicated page must contain the existing complexity controls: {text:?}"
        );

        overview.click();
        next_tick().await;

        // The configurable backend's card links to its settings page.
        find_button_by_text(&container, "Configure Hermes")
            .expect("a configurable backend's card must offer a Configure action")
            .click();
        next_tick().await;

        assert_eq!(
            panel_title(&container),
            "Hermes",
            "Configure must open the backend's own settings page"
        );
        assert_eq!(
            container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            3,
            "the backend page renders one control per schema field"
        );
    }

    #[wasm_bindgen_test]
    async fn backend_cards_embed_subscription_capacity_and_hide_unsupported_backends() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Claude, BackendKind::Hermes],
            );
            state.backend_capacity.update(|hosts| {
                let snapshots = hosts.entry("host-cfg".to_owned()).or_default();
                snapshots.insert(
                    BackendKind::Claude,
                    protocol::BackendCapacitySnapshot {
                        backend_kind: BackendKind::Claude,
                        state: protocol::BackendCapacityState::Known {
                            report: protocol::CapacityReport {
                                source: protocol::CapacitySource::ClaudeRateLimitEvent,
                                observed_at_ms: None,
                                plan: None,
                                buckets: vec![protocol::CapacityBucket {
                                    id: protocol::CapacityBucketId::Claude {
                                        limit: protocol::ClaudeLimitType::FiveHour,
                                    },
                                    label: "session limit".to_owned(),
                                    measure: protocol::CapacityMeasure::UsedPercent {
                                        used_percent: 20,
                                        remaining_percent: 80,
                                        provenance: protocol::ValueProvenance {
                                            vendor_reported: true,
                                        },
                                    },
                                    scope: protocol::CapacityScope::NotReported,
                                    window: protocol::CapacityWindow::NotReported,
                                    reset: protocol::CapacityReset::NotReported,
                                    status: Some(protocol::CapacityBucketStatus::Allowed),
                                }],
                                coverage: protocol::CapacityCoverage::RepresentativeBucketOnly,
                            },
                        },
                        retrieved_at_ms: js_sys::Date::now() as u64,
                        freshness: protocol::CapacityFreshness::Fresh { age_ms: 0 },
                    },
                );
                snapshots.insert(
                    BackendKind::Hermes,
                    protocol::BackendCapacitySnapshot {
                        backend_kind: BackendKind::Hermes,
                        state: protocol::BackendCapacityState::Unsupported {
                            reason: protocol::CapacityUnsupportedReason::BackendHasNoCapacitySource,
                        },
                        retrieved_at_ms: js_sys::Date::now() as u64,
                        freshness: protocol::CapacityFreshness::Fresh { age_ms: 0 },
                    },
                );
            });
            provide_context(state);
            view! { <BackendsTab active_page=RwSignal::new(SettingsPage::Tab(SettingsTab::Backends)) /> }
        });
        next_tick().await;

        let cards = container
            .query_selector_all(".settings-backend-card")
            .expect("backend cards");
        let mut claude_capacity = false;
        let mut hermes_capacity = false;
        for index in 0..cards.length() {
            let card: HtmlElement = cards.item(index).unwrap().dyn_into().unwrap();
            let text = card.text_content().unwrap_or_default();
            let has_capacity = card
                .query_selector(".capacity-card-embedded")
                .unwrap()
                .is_some();
            if text.contains("Claude") {
                claude_capacity = has_capacity && text.contains("20% used");
            }
            if text.contains("Hermes") {
                hermes_capacity = has_capacity;
            }
        }
        assert!(
            claude_capacity,
            "Claude's subscription usage must be embedded in its backend card"
        );
        assert!(
            !hermes_capacity,
            "backends without subscription capacity must render no capacity block"
        );
        assert!(
            !container
                .text_content()
                .unwrap_or_default()
                .contains("Subscription capacity"),
            "the old standalone subscription section must be removed"
        );
    }

    /// A disabled backend still gets its page (never filtered by
    /// `enabled_backends`), renders an explicit disabled state with its schema
    /// fields visible but locked — some backends persist config edits to the
    /// native backend immediately, so an edit while disabled would fail — and
    /// offers an enable action that commits an `EnabledBackends` SetSetting
    /// preserving already-enabled backends.
    #[wasm_bindgen_test]
    async fn backend_page_disabled_backend_shows_enable_action() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Claude],
            );
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        click_tab(&container, "Hermes");
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("disabled on the selected host"),
            "a disabled backend's page must state the disabled condition explicitly: {text:?}"
        );
        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        assert_eq!(
            inputs.length(),
            3,
            "schema fields still render so the user can see the configuration"
        );
        for i in 0..inputs.length() {
            let input: HtmlInputElement = inputs.item(i).unwrap().dyn_into().unwrap();
            assert!(
                input.disabled(),
                "config controls must be locked while the backend is disabled"
            );
        }

        // Even a synthetic change event on a locked control must not reach the
        // wire — the edit would fail server-side for natively-persisted
        // backends.
        let first: HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        set_and_change(&first, "should-not-commit");
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            last_backend_config(&calls).is_none(),
            "a locked config field must never emit a backend_config frame"
        );

        find_button_by_text(&container, "Enable backend")
            .expect("the disabled state must offer an enable action")
            .click();
        for _ in 0..4 {
            next_tick().await;
        }

        let settings = recorded_set_setting_payloads(&calls);
        let enabled = settings
            .iter()
            .rev()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("enabled_backends"))
            .expect("Enable backend must emit an EnabledBackends SetSetting frame");
        let list: Vec<&str> = enabled
            .get("enabled_backends")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            list.contains(&"hermes"),
            "the enable action must enable this backend: {list:?}"
        );
        assert!(
            list.contains(&"claude"),
            "already-enabled backends must be preserved: {list:?}"
        );
    }

    /// Tycode persists `BackendConfig` edits to the native backend right away,
    /// so a page for a Tycode-like backend that is disabled and not installed
    /// must lock every config control — no edit frame can reach the wire, even
    /// from synthetic events — while keeping the Enable action live. Once the
    /// server reports the backend enabled and installed, the controls unlock
    /// and edits commit normally.
    #[wasm_bindgen_test]
    async fn tycode_page_locks_config_until_enabled_and_installed() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let state = AppState::new();
        let host_id = "host-tyc".to_owned();
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
                    enabled_backends: vec![BackendKind::Claude],
                    default_backend: Some(BackendKind::Claude),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    supervisor: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
        state.backend_config_schemas.update(|m| {
            m.entry(host_id.clone())
                .or_default()
                .insert(BackendKind::Tycode, tycode_config_schema());
        });
        state.backend_setup_by_host.update(|m| {
            m.insert(
                host_id.clone(),
                vec![backend_setup_info(
                    BackendKind::Tycode,
                    BackendSetupStatus::NotInstalled,
                )],
            );
        });
        state.settings_open.set(true);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        click_tab(&container, "Tycode");
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("disabled on the selected host"),
            "the locked page must state the disabled condition: {text:?}"
        );
        assert!(
            text.contains("not installed"),
            "the locked page must state the not-installed condition: {text:?}"
        );
        assert!(
            text.contains("read-only"),
            "the locked page must say the settings are read-only: {text:?}"
        );

        let select: HtmlSelectElement = container
            .query_selector(".settings-backend-config-fields select")
            .unwrap()
            .expect("the Tycode provider select must render from the schema")
            .dyn_into()
            .unwrap();
        assert!(
            select.disabled(),
            "the select control must be locked while disabled and not installed"
        );
        let input: HtmlInputElement = container
            .query_selector("input.settings-backend-config-input")
            .unwrap()
            .expect("the Tycode text field must render from the schema")
            .dyn_into()
            .unwrap();
        assert!(input.disabled(), "the text control must be locked");

        // Locked controls must never reach the wire, even via synthetic events
        // that bypass the disabled attribute.
        select.set_value("bedrock");
        dispatch_event_from_js(&select.clone().unchecked_into(), "change", None);
        let _ = select.remove_attribute("id");
        set_and_change(&input, "work");
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            last_backend_config(&calls).is_none(),
            "a locked page must not emit any backend_config frame"
        );

        // The enable path stays live from the locked page.
        let enable = find_button_by_text(&container, "Enable backend")
            .expect("the locked page must keep the enable action available");
        assert!(
            !enable.has_attribute("disabled"),
            "the enable action itself must not be locked"
        );
        enable.click();
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            recorded_set_setting_payloads(&calls)
                .iter()
                .any(|s| s.get("kind").and_then(|k| k.as_str()) == Some("enabled_backends")),
            "the enable action must emit an EnabledBackends SetSetting frame"
        );

        // Server confirms the backend enabled and installed → controls unlock.
        state.host_settings_by_host.update(|m| {
            if let Some(settings) = m.get_mut(&host_id) {
                settings.enabled_backends = vec![BackendKind::Claude, BackendKind::Tycode];
            }
        });
        state.backend_setup_by_host.update(|m| {
            m.insert(
                host_id.clone(),
                vec![backend_setup_info(
                    BackendKind::Tycode,
                    BackendSetupStatus::Installed,
                )],
            );
        });
        for _ in 0..3 {
            next_tick().await;
        }

        let select: HtmlSelectElement = container
            .query_selector(".settings-backend-config-fields select")
            .unwrap()
            .expect("the select must re-render after the server state change")
            .dyn_into()
            .unwrap();
        assert!(
            !select.disabled(),
            "controls must unlock once the backend is enabled and installed"
        );
        select.set_value("bedrock");
        dispatch_event_from_js(&select.clone().unchecked_into(), "change", None);
        for _ in 0..3 {
            next_tick().await;
        }
        let setting =
            last_backend_config(&calls).expect("an unlocked edit must emit a backend_config frame");
        assert_eq!(
            setting.get("backend").and_then(|b| b.as_str()),
            Some("tycode"),
            "the edit must target Tycode: {setting:?}"
        );
        let values = setting
            .get("values")
            .and_then(|v| v.as_object())
            .expect("values object");
        assert_eq!(
            values
                .get("active_provider")
                .and_then(|v| v.get("string"))
                .and_then(|s| s.as_str()),
            Some("bedrock"),
            "the unlocked edit must carry the typed value: {values:?}"
        );
    }

    /// A `TydeSettingsStore` backend's config lives in Tyde host settings, not
    /// in the backend itself, so an enabled backend stays editable even while
    /// its setup probe reports Unavailable — users need exactly these settings
    /// to recover such a backend. Edits must still emit typed
    /// `backend_config` frames.
    #[wasm_bindgen_test]
    async fn tyde_store_page_stays_editable_when_setup_unavailable() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            // The setup probe can't reach the backend, but the schema's
            // persistence mode is TydeSettingsStore — controls stay live.
            state.backend_setup_by_host.update(|m| {
                m.insert(
                    "host-cfg".to_owned(),
                    vec![backend_setup_info(
                        BackendKind::Hermes,
                        BackendSetupStatus::Unavailable,
                    )],
                );
            });
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("read-only"),
            "an enabled TydeSettingsStore backend must not be locked by setup status: {text:?}"
        );

        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        assert_eq!(inputs.length(), 3, "schema fields must render");
        for i in 0..inputs.length() {
            let input: HtmlInputElement = inputs.item(i).unwrap().dyn_into().unwrap();
            assert!(
                !input.disabled(),
                "controls must stay editable while the backend is enabled"
            );
        }

        // Schema field order: default_model (0), default_provider (1).
        let provider: HtmlInputElement = inputs.item(1).unwrap().dyn_into().unwrap();
        set_and_change(&provider, "openrouter");
        for _ in 0..3 {
            next_tick().await;
        }

        let setting = last_backend_config(&calls)
            .expect("an edit on an editable TydeSettingsStore page must reach the wire");
        assert_eq!(
            setting.get("backend").and_then(|b| b.as_str()),
            Some("hermes"),
            "the edit must target the schema's backend: {setting:?}"
        );
        assert_eq!(
            setting
                .get("values")
                .and_then(|v| v.get("default_provider"))
                .and_then(|v| v.get("string"))
                .and_then(|s| s.as_str()),
            Some("openrouter"),
            "the edit must carry the typed value: {setting:?}"
        );
    }

    /// A backend whose CLI is found but unusable is reported `Unavailable`
    /// with a server-owned diagnostic telling the user to repair the install.
    /// The Backends overview card must pair that diagnostic with a runnable
    /// repair affordance — hiding the install command would tell the user to
    /// re-run the installer while offering no way to do it.
    #[wasm_bindgen_test]
    async fn backend_card_unavailable_offers_repair_install() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let diagnostic_message =
            "Hermes CLI found but unusable: re-run the Hermes installer or set HERMES_PYTHON";
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            state.backend_setup_by_host.update(|m| {
                m.insert(
                    "host-cfg".to_owned(),
                    vec![protocol::BackendSetupInfo {
                        backend_kind: BackendKind::Hermes,
                        status: BackendSetupStatus::Unavailable,
                        installed_version: None,
                        docs_url: "https://example.test/docs".to_owned(),
                        install_command: Some(protocol::BackendSetupCommand {
                            title: "Install Hermes".to_owned(),
                            description: "Installs the Hermes CLI".to_owned(),
                            command: "hermes-installer".to_owned(),
                            display_command: None,
                            runnable: true,
                        }),
                        diagnostic: Some(protocol::BackendSetupDiagnostic {
                            code: protocol::BackendSetupDiagnosticCode::GatewayImportFailed,
                            message: diagnostic_message.to_owned(),
                        }),
                        sign_in_command: None,
                    }],
                );
            });
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        find_button_by_text(&container, "Overview")
            .expect("the Backends group must have an Overview item")
            .click();
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Unavailable"),
            "the card must report the server's Unavailable status: {text:?}"
        );
        assert!(
            !text.contains("Not installed"),
            "a found-but-unusable install must not be misreported as not installed: {text:?}"
        );
        assert!(
            text.contains(diagnostic_message),
            "the server-owned diagnostic must stay visible verbatim: {text:?}"
        );

        assert!(
            find_button_by_text(&container, "Install").is_none(),
            "an Unavailable backend must offer a repair action, not a fresh Install"
        );
        let repair = find_button_by_text(&container, "Repair install")
            .expect("an Unavailable backend with an install command must offer a repair action");
        assert!(
            !repair.has_attribute("disabled"),
            "a runnable install command must keep the repair action live"
        );

        repair.click();
        for _ in 0..3 {
            next_tick().await;
        }

        let setup_frames: Vec<serde_json::Value> = calls
            .iter()
            .filter_map(|entry| entry.dyn_into::<js_sys::Array>().ok())
            .filter(|arr| arr.get(0).as_string().as_deref() == Some("send_host_line"))
            .filter_map(|arr| arr.get(1).as_string())
            .filter_map(|args_json| serde_json::from_str::<serde_json::Value>(&args_json).ok())
            .filter_map(|args| {
                args.get("line")
                    .and_then(|v| v.as_str())
                    .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            })
            .filter(|envelope| {
                envelope.get("kind").and_then(|v| v.as_str()) == Some("run_backend_setup")
            })
            .collect();
        assert_eq!(
            setup_frames.len(),
            1,
            "clicking the repair action must fire exactly one RunBackendSetup frame"
        );
        let payload = setup_frames[0]
            .get("payload")
            .expect("run_backend_setup frame must carry a payload");
        assert_eq!(
            payload.get("backend_kind").and_then(|v| v.as_str()),
            Some("hermes"),
            "the repair action must target this card's backend: {payload:?}"
        );
        assert_eq!(
            payload.get("action").and_then(|v| v.as_str()),
            Some("install"),
            "the repair action must run the server's install command: {payload:?}"
        );
    }

    /// When the selected host changes to one whose schema catalog no longer
    /// carries the active backend page, the panel falls back to Overview and
    /// the stale nav item disappears — no stale child list, no blank page.
    #[wasm_bindgen_test]
    async fn backend_page_falls_back_to_overview_when_host_changes() {
        let container = make_container();
        let state = AppState::new();
        install_backend_config_host(
            &state,
            BackendConfigValues::default(),
            vec![BackendKind::Hermes],
        );
        state.settings_open.set(true);
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        click_tab(&container, "Hermes");
        next_tick().await;
        assert_eq!(panel_title(&container), "Hermes");

        state.selected_host_id.set(Some("host-other".to_owned()));
        for _ in 0..3 {
            next_tick().await;
        }

        assert_eq!(
            panel_title(&container),
            "Backends",
            "losing the schema must land the user on the Backends overview"
        );
        assert!(
            find_button_by_text(&container, "Hermes").is_none(),
            "the stale backend nav item must not linger after the host change"
        );
    }

    /// The existing "Backends" deep link (e.g. the onboarding CTA) opens the
    /// Backends overview page.
    #[wasm_bindgen_test]
    async fn settings_deep_link_opens_backends_overview() {
        let container = make_container();
        let state = AppState::new();
        state.settings_open.set(true);
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        state.settings_tab_request.set(Some("Backends"));
        for _ in 0..3 {
            next_tick().await;
        }

        assert_eq!(
            panel_title(&container),
            "Backends",
            "the Backends deep link must open the overview page"
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Default Backend"),
            "the overview content must render after the deep link: {text:?}"
        );
    }

    /// Settings search matches backend pages by their server-provided schema
    /// field labels, and filters out unrelated tabs (including Overview when
    /// only a backend page matches).
    #[wasm_bindgen_test]
    async fn settings_search_matches_backend_page_fields() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        let search: web_sys::HtmlInputElement = container
            .query_selector(".settings-search-input")
            .unwrap()
            .expect("settings search input must render")
            .dyn_into()
            .unwrap();
        search.set_value("default provider");
        dispatch_event_from_js(&search, "input", None);
        next_tick().await;

        assert!(
            find_button_by_text(&container, "Hermes").is_some(),
            "a backend page must match a search for one of its schema field labels"
        );
        assert!(
            find_button_by_text(&container, "Appearance").is_none(),
            "non-matching settings tabs must be filtered out"
        );
        assert!(
            find_button_by_text(&container, "Overview").is_none(),
            "Overview must be filtered when the query only matches a backend page"
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
                    supervisor: Default::default(),
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
                            select_options_by_setting: None,
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

    // ---- Backend-native (Tycode) settings page ----

    /// Install a connected host with Tycode enabled but *no* legacy deep-config
    /// schema, plus a caller-supplied backend-native settings snapshot, and
    /// select it — the setup for exercising the Tycode native settings page.
    fn install_tycode_native_host(state: &AppState, snapshot: BackendNativeSettingsSnapshot) {
        let host_id = "host-tyc-native".to_owned();
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
                host_settings_with_hermes_config(
                    std::collections::HashMap::new(),
                    vec![BackendKind::Tycode],
                ),
            );
        });
        state.backend_setup_by_host.update(|m| {
            m.insert(
                host_id.clone(),
                vec![backend_setup_info(
                    BackendKind::Tycode,
                    BackendSetupStatus::Installed,
                )],
            );
        });
        state.backend_native_settings.update(|m| {
            m.entry(host_id)
                .or_default()
                .insert(BackendKind::Tycode, snapshot);
        });
    }

    /// Wrap one profile's settings object into the Tycode profiles document
    /// shape the server publishes (see `protocol::tycode_config`): version,
    /// then one `default` profile backed by the shared settings file.
    fn tycode_profiles_doc(settings: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "profiles": [{
                "name": "default",
                "settings_path": "/home/user/.tycode/settings.toml",
                "settings": settings,
            }],
        })
    }

    /// A Ready snapshot with a top-level Core group (an enum control) and a
    /// nested Module group carrying a secret `api_key` and a plain `model`.
    fn tycode_ready_snapshot() -> BackendNativeSettingsSnapshot {
        let settings = serde_json::json!({
            "active_provider": "anthropic",
            "providers": {
                "anthropic": { "api_key": "sk-secret-value", "model": "claude" }
            }
        });
        BackendNativeSettingsSnapshot {
            backend_kind: BackendKind::Tycode,
            status: BackendConfigSnapshotStatus::Ready,
            settings: Some(tycode_profiles_doc(settings)),
            groups: vec![
                BackendNativeSettingsGroup {
                    id: "core".to_owned(),
                    title: "Core".to_owned(),
                    kind: BackendNativeSettingsGroupKind::Core,
                    settings_path: Vec::new(),
                    description: Some("Top-level settings".to_owned()),
                    schema: serde_json::json!({
                        "properties": {
                            "active_provider": {
                                "type": "string",
                                "title": "Active Provider",
                                "enum": ["anthropic", "bedrock"]
                            }
                        }
                    }),
                },
                BackendNativeSettingsGroup {
                    id: "anthropic".to_owned(),
                    title: "Anthropic".to_owned(),
                    kind: BackendNativeSettingsGroupKind::Module,
                    settings_path: vec!["providers".to_owned(), "anthropic".to_owned()],
                    description: None,
                    schema: serde_json::json!({
                        "properties": {
                            "api_key": { "type": "string", "title": "API Key" },
                            "model": { "type": "string", "title": "Model" }
                        }
                    }),
                },
            ],
            message: None,
            advisories: Vec::new(),
        }
    }

    // ── Tycode native-settings advisory fixtures ────────────────────────────
    //
    // Every value here is server-owned typed state. The UI must render these and
    // nothing else — it may not parse the settings document or read the message
    // text.

    /// The Ready grouped snapshot plus any server-owned advisories. The settings
    /// document is unchanged, so the existing masking, grouping, and save
    /// behaviour is exercised alongside the advisories.
    fn tycode_advisory_snapshot(
        advisories: Vec<BackendNativeSettingsAdvisory>,
    ) -> BackendNativeSettingsSnapshot {
        BackendNativeSettingsSnapshot {
            advisories,
            ..tycode_ready_snapshot()
        }
    }

    /// Replace the Tycode native snapshot the server has published, exactly as a
    /// refreshed `BackendConfigSnapshots` frame would.
    fn publish_tycode_snapshot(state: &AppState, snapshot: BackendNativeSettingsSnapshot) {
        state.backend_native_settings.update(|m| {
            m.entry("host-tyc-native".to_owned())
                .or_default()
                .insert(BackendKind::Tycode, snapshot);
        });
    }

    /// The host id `install_tycode_native_host` installs on, and selects.
    const TYCODE_HOST: &str = "host-tyc-native";

    /// A typed `CommandError`, dispatched through the real dispatcher exactly as the
    /// server sends one.
    ///
    /// `seq` must start at 0 for each host and advance by one: `prime_host_for_tests`
    /// forgets the host's counters, and the inbound validator **drops** any envelope
    /// whose seq is not the one it expects (and marks the connection desynced), so an
    /// off-by-one here silently delivers nothing.
    fn dispatch_command_error(
        state: &AppState,
        host_id: &str,
        seq: u64,
        request_kind: FrameKind,
        setting_target: Option<HostSettingErrorTarget>,
        code: CommandErrorCode,
        message: &str,
    ) {
        let stream = protocol::StreamPath(format!("/host/{host_id}"));
        let envelope = protocol::Envelope::from_payload(
            stream.clone(),
            FrameKind::CommandError,
            seq,
            &protocol::CommandErrorPayload {
                stream,
                request_kind,
                setting_target,
                operation: "set_setting".to_owned(),
                code,
                message: message.to_owned(),
                fatal: false,
            },
        )
        .expect("envelope serialize");
        crate::dispatch::dispatch_envelope(state, host_id, envelope);
    }

    /// The native-save state the dispatcher recorded for a host's Tycode backend.
    fn native_save_state(state: &AppState, host_id: &str) -> Option<NativeSettingsSaveState> {
        state
            .native_settings_save_state
            .get_untracked()
            .get(host_id)
            .and_then(|by_kind| by_kind.get(&BackendKind::Tycode))
            .cloned()
    }

    /// Mark a native save in flight, exactly as the settings page does when it sends
    /// one: `base` is the settings document the save was applied to.
    fn mark_native_save_pending(state: &AppState, host_id: &str) {
        let base = state
            .backend_native_settings
            .get_untracked()
            .get(host_id)
            .and_then(|by_kind| by_kind.get(&BackendKind::Tycode))
            .and_then(|snapshot| snapshot.settings.clone())
            .expect("the host must have an installed snapshot to save against");
        state.native_settings_save_state.update(|states| {
            states.entry(host_id.to_owned()).or_default().insert(
                BackendKind::Tycode,
                NativeSettingsSaveState::Pending { base },
            );
        });
    }

    fn confirm_dialog(container: &HtmlElement) -> Option<HtmlElement> {
        container
            .query_selector(".settings-confirm-modal")
            .unwrap()
            .map(|el| el.dyn_into().unwrap())
    }

    // ── Destructive-confirmation accessibility ──────────────────────────────
    //
    // `SettingsConfirmDialog` is shared by every irreversible settings action:
    // deleting a launch profile, a custom agent, an MCP server, or a steering
    // entry. The contract below is asserted against the component's callers,
    // because it is the *component* that has to hold it, not one caller.

    fn active_element() -> Option<web_sys::Element> {
        web_sys::window()?.document()?.active_element()
    }

    fn is_focused(el: &HtmlElement) -> bool {
        active_element().is_some_and(|active| active.is_same_node(Some(el.unchecked_ref())))
    }

    fn focused_label() -> String {
        active_element()
            .map(|el| el.text_content().unwrap_or_default())
            .unwrap_or_else(|| "<nothing>".to_owned())
    }

    /// Installs the app's real global listeners for the life of the guard.
    ///
    /// They live in thread-locals shared by the whole wasm test binary, so a leaked
    /// listener keeps firing against a dead `AppState` in every later test. Cleanup
    /// therefore cannot be a bare call at the end of the test body: any failing
    /// assertion jumps straight past it. Tying it to `Drop` means the teardown runs
    /// on every path out of the scope, including an early `return` or `?`.
    ///
    /// Installing also clears first, so a guard is a clean slate regardless of what
    /// ran before it.
    struct GlobalListeners;

    impl GlobalListeners {
        fn install(state: &AppState) -> Self {
            crate::app::clear_app_listeners();
            crate::app::install_keydown_listener(
                state.clone(),
                crate::components::center_zone::workspace_width(),
            );
            Self
        }
    }

    impl Drop for GlobalListeners {
        fn drop(&mut self) {
            crate::app::clear_app_listeners();
        }
    }

    fn key_event(key: &str, shift: bool) -> web_sys::KeyboardEvent {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(key);
        init.set_shift_key(shift);
        init.set_bubbles(true);
        init.set_cancelable(true);
        web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap()
    }

    /// The dialog is announced as a modal, its title and warning *are* its
    /// accessible name and description, and focus has moved onto the safe control.
    ///
    /// Returns `(cancel, confirm)` for the keyboard checks that follow.
    fn assert_confirm_dialog_is_an_accessible_modal(
        container: &HtmlElement,
        expected_title: &str,
        flow: &str,
    ) -> (HtmlElement, HtmlElement) {
        let dialog = confirm_dialog(container)
            .unwrap_or_else(|| panic!("{flow}: the destructive action must open a confirmation"));

        assert_eq!(
            dialog.get_attribute("role").as_deref(),
            Some("alertdialog"),
            "{flow}: an irreversible confirmation must announce itself as an alert dialog"
        );
        assert_eq!(
            dialog.get_attribute("aria-modal").as_deref(),
            Some("true"),
            "{flow}: the confirmation covers the page and traps focus, so it must say it is modal"
        );

        // An `aria-labelledby` pointing at nothing announces nothing, so resolve
        // both ids against the real DOM rather than just checking they exist.
        let labelled_by = dialog
            .get_attribute("aria-labelledby")
            .unwrap_or_else(|| panic!("{flow}: the confirmation must have an accessible name"));
        let described_by = dialog.get_attribute("aria-describedby").unwrap_or_else(|| {
            panic!("{flow}: the confirmation must have an accessible description")
        });
        let title_el = container
            .query_selector(&format!("#{labelled_by}"))
            .unwrap()
            .unwrap_or_else(|| panic!("{flow}: aria-labelledby must resolve to a real element"));
        let description_el = container
            .query_selector(&format!("#{described_by}"))
            .unwrap()
            .unwrap_or_else(|| panic!("{flow}: aria-describedby must resolve to a real element"));
        assert_eq!(
            title_el.text_content().as_deref().map(str::trim),
            Some(expected_title),
            "{flow}: the accessible name must be the dialog's own title"
        );
        assert!(
            !description_el
                .text_content()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "{flow}: the accessible description must be the warning, not an empty node"
        );

        // Both scoped to the dialog: the page underneath has its own Cancel and its
        // own danger buttons (the row's "Delete" is a `settings-btn-danger` too).
        let cancel = find_button_by_text(&dialog, "Cancel")
            .unwrap_or_else(|| panic!("{flow}: the confirmation must offer a way out"));
        let confirm: HtmlElement = dialog
            .query_selector(".settings-btn-danger")
            .unwrap()
            .unwrap_or_else(|| panic!("{flow}: the destructive button must be marked as such"))
            .dyn_into()
            .unwrap();

        // Focus is inside the dialog, and it is on the *safe* control. This is the
        // whole point: previously focus stayed on the button behind the modal, so a
        // keyboard user was never told anything had opened and could tab straight
        // onto a `settings-btn-danger` and confirm an irreversible action blind.
        assert!(
            is_focused(&cancel),
            "{flow}: focus must move to Cancel when the confirmation opens \u{2014} never to \
             the destructive button, and never left behind on the page underneath. Focus is \
             on: {:?}",
            focused_label()
        );

        (cancel, confirm)
    }

    /// Tab cannot walk out of a modal that is covering the page, and Escape
    /// cancels from wherever focus actually is.
    async fn assert_focus_is_trapped_and_escape_cancels(
        container: &HtmlElement,
        cancel: &HtmlElement,
        confirm: &HtmlElement,
        flow: &str,
    ) {
        confirm.focus().unwrap();
        confirm.dispatch_event(&key_event("Tab", false)).unwrap();
        next_tick().await;
        assert!(
            is_focused(cancel),
            "{flow}: Tab off the last control must wrap to Cancel, not leave the dialog. \
             Focus is on: {:?}",
            focused_label()
        );

        cancel.focus().unwrap();
        cancel.dispatch_event(&key_event("Tab", true)).unwrap();
        next_tick().await;
        assert!(
            is_focused(confirm),
            "{flow}: Shift+Tab off the first control must wrap to the last, not leave the \
             dialog. Focus is on: {:?}",
            focused_label()
        );

        // Escape has to work from where focus *is*. It used to be handled on the
        // overlay, which nothing ever focused, so it did nothing at all.
        cancel.focus().unwrap();
        cancel.dispatch_event(&key_event("Escape", false)).unwrap();
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            confirm_dialog(container).is_none(),
            "{flow}: Escape must close the confirmation"
        );
    }

    fn native_inputs_enabled(container: &HtmlElement) -> bool {
        let inputs = container
            .query_selector_all(".settings-native-input")
            .unwrap();
        assert!(inputs.length() > 0, "expected native inputs to render");
        (0..inputs.length()).all(|i| {
            let input: HtmlInputElement = inputs.item(i).unwrap().dyn_into().unwrap();
            !input.disabled()
        })
    }

    /// Most recent `backend_native_settings` SetSetting payload, if any.
    fn last_native_settings(calls: &js_sys::Array) -> Option<serde_json::Value> {
        recorded_set_setting_payloads(calls)
            .into_iter()
            .rev()
            .find(|s| s.get("kind").and_then(|k| k.as_str()) == Some("backend_native_settings"))
    }

    /// When Tycode's native settings probe fails, the Tycode page appears in
    /// the Backends sidebar and shows the server's own reason verbatim — never
    /// blank/default value controls.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_unavailable_shows_server_message() {
        let container = make_container();
        let message = "Tycode native settings probe timed out waiting for SettingsSchema";
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Unavailable,
                    settings: None,
                    groups: Vec::new(),
                    message: Some(message.to_owned()),
                    advisories: Vec::new(),
                },
            );
            state.settings_open.set(true);
            provide_context(state);
            view! { <SettingsPanel /> }
        });
        next_tick().await;

        // The native snapshot alone (no legacy schema) must earn a sidebar page.
        click_tab(&container, "Tycode");
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains(message),
            "the server's unavailable reason must be surfaced verbatim: {text:?}"
        );
        assert_eq!(
            container
                .query_selector_all("input.settings-native-input")
                .unwrap()
                .length(),
            0,
            "unavailable native settings must not render blank/default value controls"
        );
    }

    /// A Ready snapshot renders each server-provided group with its current
    /// values seeded from the settings document — a top-level enum and nested
    /// module fields — and never invents defaults.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_render_grouped_current_values() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Core") && text.contains("Anthropic"),
            "both server-provided groups must render, none dropped: {text:?}"
        );

        // The top-level enum control reflects the current settings value.
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must render for active_provider")
            .dyn_into()
            .unwrap();
        assert_eq!(
            select.value(),
            "anthropic",
            "the enum control must seed from the current settings value"
        );

        // The nested `model` text field reflects the current value.
        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the model text control must render")
            .dyn_into()
            .unwrap();
        assert_eq!(
            model.value(),
            "claude",
            "a nested field must seed from the value at its group's settings_path"
        );
    }

    /// Secret-named keys (`api_key`) render as masked password inputs and their
    /// value is never placed in the DOM — no secret leakage into the page.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_mask_secret_keys() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let secret: HtmlInputElement = container
            .query_selector("input[type=\"password\"].settings-native-input")
            .unwrap()
            .expect("a secret-named key must render as a masked password input")
            .dyn_into()
            .unwrap();
        assert_eq!(
            secret.value(),
            "",
            "the stored secret value must never be pre-filled into the control"
        );
        let html = container.inner_html();
        assert!(
            !html.contains("sk-secret-value"),
            "the secret value must not appear anywhere in the rendered page"
        );
    }

    /// Editing a native settings field sends `BackendNativeSettings` carrying the
    /// full updated settings object (not a partial patch) so the backend can
    /// `SaveSettings { persist: true }`. Sibling values, including untouched
    /// secrets, are preserved in the sent document.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_save_sends_full_object() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the model text control must render")
            .dyn_into()
            .unwrap();
        set_and_change(&model, "opus");
        for _ in 0..3 {
            next_tick().await;
        }

        let setting = last_native_settings(&calls)
            .expect("an edit must emit a backend_native_settings frame");
        assert_eq!(
            setting.get("backend").and_then(|b| b.as_str()),
            Some("tycode"),
            "the edit must target the Tycode backend: {setting:?}"
        );
        // The wire document is the profiles document: the edit lands inside
        // the selected profile's settings, with the unedited settings echoed
        // as `base_settings` so the server can refuse a stale save.
        let settings = setting.get("settings").expect("the full profiles document");
        assert_eq!(
            settings
                .pointer("/profiles/0/settings/providers/anthropic/model")
                .and_then(|v| v.as_str()),
            Some("opus"),
            "the edited nested value must be updated in place: {settings:?}"
        );
        assert_eq!(
            settings
                .pointer("/profiles/0/settings/active_provider")
                .and_then(|v| v.as_str()),
            Some("anthropic"),
            "sibling top-level values must be preserved in the full object: {settings:?}"
        );
        assert_eq!(
            settings
                .pointer("/profiles/0/settings/providers/anthropic/api_key")
                .and_then(|v| v.as_str()),
            Some("sk-secret-value"),
            "an untouched sibling secret must be preserved in the full object: {settings:?}"
        );
        assert_eq!(
            settings
                .pointer("/profiles/0/base_settings/providers/anthropic/model")
                .and_then(|v| v.as_str()),
            Some("claude"),
            "the unedited settings must ride along as base_settings for the \
             server's stale-save refusal: {settings:?}"
        );
    }

    /// A multi-profile Tycode document renders one chip per profile (default
    /// first), shows the selected profile's settings file, scopes edits to
    /// that profile, and offers deletion only for named profiles.
    #[wasm_bindgen_test]
    async fn tycode_profiles_render_chips_and_scope_edits_to_the_selected_profile() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let doc = serde_json::json!({
                "version": 1,
                "profiles": [
                    {
                        "name": "default",
                        "settings_path": "/home/user/.tycode/settings.toml",
                        "settings": {
                            "active_provider": "anthropic",
                            "providers": {
                                "anthropic": { "api_key": "sk-a", "model": "claude" }
                            }
                        },
                    },
                    {
                        "name": "work",
                        "settings_path": "/home/user/.tycode/profiles/work.toml",
                        "settings": {
                            "active_provider": "anthropic",
                            "providers": {
                                "anthropic": { "api_key": "sk-b", "model": "haiku" }
                            }
                        },
                    },
                ],
            });
            let snapshot = BackendNativeSettingsSnapshot {
                settings: Some(doc),
                ..tycode_ready_snapshot()
            };
            install_tycode_native_host(&state, snapshot);
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // One chip per profile, default first, plus the create control.
        let chips = container
            .query_selector_all(".settings-native-profile-chip")
            .unwrap();
        let chip_texts: Vec<String> = (0..chips.length())
            .map(|i| chips.item(i).unwrap().text_content().unwrap_or_default())
            .collect();
        assert_eq!(chip_texts, ["default", "work", "+ New profile"]);

        // The default profile is selected: its settings file is shown and the
        // default profile cannot be deleted.
        let path_text = container
            .query_selector(".settings-native-profile-path")
            .unwrap()
            .expect("the selected profile's settings file renders")
            .text_content()
            .unwrap_or_default();
        assert_eq!(path_text, "/home/user/.tycode/settings.toml");
        assert!(
            container
                .query_selector(".settings-native-profile-delete")
                .unwrap()
                .is_none(),
            "the default profile must not offer deletion"
        );

        // Switching chips swaps the rendered profile: the work profile shows
        // its own file, its own values, and a delete affordance.
        let work_chip: HtmlElement = chips.item(1).unwrap().dyn_into().unwrap();
        work_chip.click();
        next_tick().await;
        let path_text = container
            .query_selector(".settings-native-profile-path")
            .unwrap()
            .expect("the work profile's settings file renders")
            .text_content()
            .unwrap_or_default();
        assert_eq!(path_text, "/home/user/.tycode/profiles/work.toml");
        assert!(
            container
                .query_selector(".settings-native-profile-delete")
                .unwrap()
                .is_some(),
            "a named profile offers deletion"
        );
        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the work profile's model control renders")
            .dyn_into()
            .unwrap();
        assert_eq!(
            model.value(),
            "haiku",
            "values come from the selected profile"
        );

        // An edit lands inside the selected profile with its base attached;
        // the other profile rides along untouched.
        set_and_change(&model, "opus");
        for _ in 0..3 {
            next_tick().await;
        }
        let setting = last_native_settings(&calls)
            .expect("a profile-scoped edit must emit a backend_native_settings frame");
        let settings = setting.get("settings").expect("the full profiles document");
        assert_eq!(
            settings
                .pointer("/profiles/1/settings/providers/anthropic/model")
                .and_then(|v| v.as_str()),
            Some("opus"),
            "the edit must land in the selected profile: {settings:?}"
        );
        assert_eq!(
            settings
                .pointer("/profiles/1/base_settings/providers/anthropic/model")
                .and_then(|v| v.as_str()),
            Some("haiku"),
            "the selected profile's unedited settings must ride along as \
             base_settings: {settings:?}"
        );
        assert_eq!(
            settings
                .pointer("/profiles/0/settings/providers/anthropic/model")
                .and_then(|v| v.as_str()),
            Some("claude"),
            "the unselected profile must be echoed unchanged: {settings:?}"
        );
        assert!(
            settings.pointer("/profiles/0/base_settings").is_none(),
            "an untouched profile must not claim an edit basis: {settings:?}"
        );
    }

    /// A group whose schema has no typed `properties` map must not be silently
    /// dropped: its whole value renders in a visible read-only JSON view.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_render_untyped_group_as_json() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(
                        serde_json::json!({ "raw": { "nested": [1, 2, 3] } }),
                    )),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "raw".to_owned(),
                        title: "Raw Module".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Module,
                        settings_path: vec!["raw".to_owned()],
                        description: None,
                        // No "properties" — an opaque schema.
                        schema: serde_json::json!({ "type": "object" }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let pre = container
            .query_selector(".settings-native-json-readonly")
            .unwrap()
            .expect("an untyped group must render as a visible JSON view, not vanish");
        let json = pre.text_content().unwrap_or_default();
        assert!(
            json.contains("nested") && json.contains('1'),
            "the group's current value must be shown, not dropped: {json:?}"
        );
    }

    /// A legacy backend (Hermes, with a typed deep-config schema and no native
    /// snapshot) is unaffected by the native settings surface: it keeps its
    /// legacy config inputs and emits `backend_config`, never the native form.
    #[wasm_bindgen_test]
    async fn hermes_legacy_page_unaffected_by_native_settings() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_backend_config_host(
                &state,
                BackendConfigValues::default(),
                vec![BackendKind::Hermes],
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Hermes /> }
        });
        next_tick().await;

        assert_eq!(
            container
                .query_selector_all("input.settings-backend-config-input")
                .unwrap()
                .length(),
            3,
            "Hermes legacy schema fields must still render"
        );
        assert!(
            container
                .query_selector(".settings-native-settings")
                .unwrap()
                .is_none(),
            "a legacy backend must not render the native settings surface"
        );

        let inputs = container
            .query_selector_all("input.settings-backend-config-input")
            .unwrap();
        let provider: HtmlInputElement = inputs.item(1).unwrap().dyn_into().unwrap();
        set_and_change(&provider, "openrouter");
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            last_backend_config(&calls).is_some(),
            "Hermes must keep persisting via backend_config, not backend_native_settings"
        );
        assert!(
            last_native_settings(&calls).is_none(),
            "a legacy backend edit must never emit a backend_native_settings frame"
        );
    }

    // ---- Native settings: secret redaction, save-locking, unset, nullable ----

    /// An object-typed property whose value contains a nested secret renders as a
    /// read-only, recursively redacted JSON view — the raw secret never reaches
    /// the DOM and the value can't be edited (which would clobber the secret).
    #[wasm_bindgen_test]
    async fn tycode_native_settings_redact_nested_secret_in_json() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(serde_json::json!({
                        "auth": { "token": "sk-super-secret", "scope": "repo" }
                    }))),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "core".to_owned(),
                        title: "Core".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Core,
                        settings_path: Vec::new(),
                        description: None,
                        schema: serde_json::json!({
                            "properties": { "auth": { "type": "object", "title": "Auth" } }
                        }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let html = container.inner_html();
        assert!(
            !html.contains("sk-super-secret"),
            "a nested secret must never reach the DOM: {html:?}"
        );
        let pre = container
            .query_selector(".settings-native-json-readonly")
            .unwrap()
            .expect("secret-bearing object must render as a read-only JSON view");
        let json = pre.text_content().unwrap_or_default();
        assert!(
            json.contains("scope") && json.contains(SECRET_REDACTION),
            "non-secret keys stay visible while the secret is redacted: {json:?}"
        );
        // No editable textarea for secret-bearing JSON — editing would clobber it.
        assert!(
            container
                .query_selector("textarea.settings-native-json-input")
                .unwrap()
                .is_none(),
            "secret-bearing JSON must not be editable"
        );
    }

    /// An opaque group (schema without `properties`) whose value contains a
    /// secret renders redacted, not raw.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_redact_secret_in_opaque_group() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(serde_json::json!({
                        "providers": { "anthropic": { "api_key": "sk-leak", "model": "claude" } }
                    }))),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "anthropic".to_owned(),
                        title: "Anthropic".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Module,
                        settings_path: vec!["providers".to_owned(), "anthropic".to_owned()],
                        description: None,
                        schema: serde_json::json!({ "type": "object" }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let html = container.inner_html();
        assert!(
            !html.contains("sk-leak"),
            "an opaque group's nested secret must never reach the DOM: {html:?}"
        );
        let pre = container
            .query_selector(".settings-native-json-readonly")
            .unwrap()
            .expect("opaque group must still render its value, redacted");
        let json = pre.text_content().unwrap_or_default();
        assert!(
            json.contains("model") && json.contains(SECRET_REDACTION),
            "the redacted view keeps non-secret keys: {json:?}"
        );
    }

    /// While a native save is in flight, every native control is disabled and a
    /// saving affordance shows — a second edit off the stale snapshot can't be
    /// made. Once the server publishes a newer snapshot the controls re-enable.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_disable_controls_while_saving() {
        let _calls = install_settings_send_stub();
        let container = make_container();
        let state = AppState::new();
        install_tycode_native_host(&state, tycode_ready_snapshot());
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the model control must render")
            .dyn_into()
            .unwrap();
        set_and_change(&model, "opus");
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_some(),
            "an in-flight save must show a saving affordance"
        );
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must render")
            .dyn_into()
            .unwrap();
        assert!(
            select.disabled(),
            "sibling native controls must lock while a save is in flight"
        );
        let inputs = container
            .query_selector_all(".settings-native-input")
            .unwrap();
        for i in 0..inputs.length() {
            let input: HtmlInputElement = inputs.item(i).unwrap().dyn_into().unwrap();
            assert!(
                input.disabled(),
                "every native input must lock while saving"
            );
        }

        // Server publishes a newer snapshot reflecting the save → controls unlock.
        state.backend_native_settings.update(|m| {
            let snapshot = m
                .get_mut("host-tyc-native")
                .and_then(|h| h.get_mut(&BackendKind::Tycode))
                .expect("snapshot present");
            snapshot.settings = Some(tycode_profiles_doc(serde_json::json!({
                "active_provider": "anthropic",
                "providers": { "anthropic": { "api_key": "sk-secret-value", "model": "opus" } }
            })));
        });
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_none(),
            "the saving affordance clears once the server confirms"
        );
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must still render")
            .dyn_into()
            .unwrap();
        assert!(
            !select.disabled(),
            "controls must unlock once a newer server snapshot arrives"
        );
    }

    /// A failed native save surfaces an explicit error and leaves controls
    /// editable so the user can retry.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_failed_save_surfaces_error() {
        let container = make_container();
        let state = AppState::new();
        install_tycode_native_host(&state, tycode_ready_snapshot());
        state.native_settings_save_state.update(|m| {
            m.entry("host-tyc-native".to_owned()).or_default().insert(
                BackendKind::Tycode,
                NativeSettingsSaveState::Failed {
                    message: "Failed to save settings. Check the connection and try again."
                        .to_owned(),
                },
            );
        });
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let error = container
            .query_selector(".settings-native-error")
            .unwrap()
            .expect("a failed save must surface an explicit error");
        assert!(
            error
                .text_content()
                .unwrap_or_default()
                .contains("Failed to save settings")
        );
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must render")
            .dyn_into()
            .unwrap();
        assert!(
            !select.disabled(),
            "controls must stay editable after a failed save so the user can retry"
        );
    }

    // ── Tycode native-settings advisories ───────────────────────────────────

    /// A snapshot with no advisories renders exactly what it rendered before:
    /// groups, controls, and no disclosure surfaces at all. The advisory UI is
    /// driven purely by the typed field, so its absence is not a state the UI
    /// invents copy for.
    #[wasm_bindgen_test]
    async fn tycode_snapshot_without_advisories_renders_no_disclosures() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        for selector in [".settings-native-disclosures", ".settings-native-advisory"] {
            assert!(
                container.query_selector(selector).unwrap().is_none(),
                "a snapshot without typed advisories must render no {selector}"
            );
        }
        assert!(
            native_inputs_enabled(&container),
            "the existing grouped controls must still render and stay editable"
        );
    }

    /// A no-provider advisory is rendered from its typed variant with the
    /// server's message verbatim, and the snapshot stays Ready and editable so
    /// the user can configure a provider.
    #[wasm_bindgen_test]
    async fn tycode_no_provider_advisory_renders_and_keeps_controls_editable() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                tycode_advisory_snapshot(vec![
                    BackendNativeSettingsAdvisory::NoProviderConfigured {
                        message: "No provider is configured. Add one to start a session."
                            .to_owned(),
                    },
                ]),
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let advisory = container
            .query_selector(".settings-native-advisory-no-provider")
            .unwrap()
            .expect("a no-provider advisory must be surfaced");
        let text = advisory.text_content().unwrap_or_default();
        assert!(
            text.contains("No provider is configured. Add one to start a session."),
            "the server's advisory message must be shown verbatim: {text:?}"
        );
        assert!(
            native_inputs_enabled(&container),
            "a no-provider advisory must leave the controls that fix it editable"
        );
    }

    /// No advisory surface may leak a secret. Every advisory renders only
    /// server-authored messages — never a value out of the settings document.
    #[wasm_bindgen_test]
    async fn tycode_advisories_never_leak_a_secret_value() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                tycode_advisory_snapshot(vec![
                    BackendNativeSettingsAdvisory::NoProviderConfigured {
                        message: "No provider is configured.".to_owned(),
                    },
                    BackendNativeSettingsAdvisory::BackendReported {
                        message: "Recoverable settings diagnostic.".to_owned(),
                    },
                ]),
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // The fixture's settings document holds `api_key: "sk-secret-value"`.
        let text = container.text_content().unwrap_or_default();
        assert!(
            !text.contains("sk-secret-value"),
            "no rendered text may contain the stored secret: {text:?}"
        );
        let inputs = container
            .query_selector_all(".settings-native-input")
            .unwrap();
        for i in 0..inputs.length() {
            let input: HtmlInputElement = inputs.item(i).unwrap().dyn_into().unwrap();
            assert_ne!(
                input.value(),
                "sk-secret-value",
                "no control may be pre-filled with the stored secret"
            );
        }
        // Both advisory surfaces really are on screen — otherwise the absence
        // above would prove nothing.
        assert_eq!(
            container
                .query_selector_all(".settings-native-advisory")
                .unwrap()
                .length(),
            2,
            "both typed advisories must render"
        );
    }

    /// A fatal settings failure is `Unavailable`, and that state comes from the
    /// server's typed `status` alone.
    ///
    /// Part one: `Unavailable` is read-only — the server's reason verbatim, zero
    /// controls, and no disclosure surfaces.
    ///
    /// Part two, the important half: a `Ready` snapshot whose message *reads*
    /// like a fatal failure is still Ready and still editable. The UI must never
    /// downgrade a snapshot by reading message text; only `status` decides.
    #[wasm_bindgen_test]
    async fn tycode_unavailable_is_read_only_and_never_inferred_from_text() {
        let fatal = "Settings integrity check failed: the configuration references a \
                     settings file that is absent.";

        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Unavailable,
                    settings: None,
                    groups: Vec::new(),
                    message: Some(fatal.to_owned()),
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains(fatal),
            "the server's fatal reason must be surfaced verbatim: {text:?}"
        );
        assert_eq!(
            container
                .query_selector_all(".settings-native-input")
                .unwrap()
                .length(),
            0,
            "a fatal settings failure is read-only — no value controls may render"
        );
        assert!(
            container
                .query_selector(".settings-native-disclosures")
                .unwrap()
                .is_none(),
            "an unavailable snapshot carries no advisories to disclose"
        );

        // The same alarming text on a Ready snapshot must change nothing: status
        // is the only thing that makes the page read-only.
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    message: Some(fatal.to_owned()),
                    ..tycode_advisory_snapshot(vec![
                        BackendNativeSettingsAdvisory::BackendReported {
                            message: fatal.to_owned(),
                        },
                    ])
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        assert!(
            native_inputs_enabled(&container),
            "a Ready snapshot stays editable no matter how fatal its message sounds — \
             only `status` may make the page read-only"
        );
        assert!(
            container
                .query_selector(".settings-native-advisory")
                .unwrap()
                .is_some(),
            "a Ready snapshot still renders its advisories"
        );
    }

    /// An advisory banner is a *polite* status region, and it stays polite
    /// across the refreshes that punctuate the remedy.
    ///
    /// The remedy is "edit the settings below", and every edit publishes a fresh
    /// snapshot. If the banner were `role="alert"`, each of those refreshes would
    /// re-announce it in full, over the top of the user who is mid-fix. There must
    /// be exactly one status region and no alert region anywhere in the
    /// disclosures — before *and* after a refresh.
    ///
    /// This originally pinned the unsupported-provider banner; that advisory was
    /// removed along with the managed-projection feature, so the same contract is
    /// asserted against the surviving no-provider advisory.
    #[wasm_bindgen_test]
    async fn tycode_advisory_banner_stays_one_polite_status_across_refresh() {
        let container = make_container();
        let state = AppState::new();
        let advisory = || {
            vec![BackendNativeSettingsAdvisory::NoProviderConfigured {
                message: "No provider is configured. Add one to start a session.".to_owned(),
            }]
        };
        install_tycode_native_host(&state, tycode_advisory_snapshot(advisory()));
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let assert_polite = |phase: &str| {
            let banner = container
                .query_selector(".settings-native-advisory-no-provider")
                .unwrap()
                .unwrap_or_else(|| panic!("the banner must render {phase}"));
            assert_eq!(
                banner.get_attribute("role").as_deref(),
                Some("status"),
                "the banner must stay a polite status region {phase}"
            );
            assert_eq!(
                container
                    .query_selector_all(".settings-native-disclosures [role=\"alert\"]")
                    .unwrap()
                    .length(),
                0,
                "no disclosure may be an assertive alert {phase} — it would re-announce \
                 over a user who is part-way through the remedy"
            );
            assert_eq!(
                container
                    .query_selector_all(".settings-native-advisory-no-provider")
                    .unwrap()
                    .length(),
                1,
                "exactly one no-provider banner {phase}"
            );
        };
        assert_polite("on first render");

        // The server republishes after an edit — the ordinary rhythm of the remedy.
        publish_tycode_snapshot(&state, tycode_advisory_snapshot(advisory()));
        for _ in 0..3 {
            next_tick().await;
        }
        assert_polite("after a post-save refresh");
        assert!(
            native_inputs_enabled(&container),
            "the remedy stays available across refreshes"
        );
    }

    /// Escape in the destructive confirmation must dismiss **one** layer.
    ///
    /// Production-shaped on purpose: the app's real global `window` keydown
    /// listener is installed alongside the panel, because that listener is half of
    /// the defect. The dialog called `prevent_default()` but not
    /// `stop_propagation()`, and the global Escape arm did not check
    /// `default_prevented()` — so a single Escape cancelled the dialog *and* set
    /// `settings_open = false`. The entire Settings overlay came down, and focus
    /// landed on `<body>`, because the opener that the dialog restores focus to
    /// had just been unmounted along with the panel.
    ///
    /// `launch_profile_delete_confirmation_is_an_accessible_modal` mounts the
    /// dialog with no global listener present, so for it there is nothing for the
    /// event to escape *to* — it cannot see this. That is the whole reason this
    /// test installs the real thing.
    #[wasm_bindgen_test]
    async fn delete_confirmation_escape_dismisses_only_the_dialog_not_the_settings_overlay() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let state = AppState::new();
        install_launch_profile_host(
            &state,
            vec![launch_profile_config(
                "hermes:claude",
                "Hermes \u{b7} Claude",
            )],
        );
        state.settings_open.set(true);

        // The real listener that owns Escape for the whole app. Torn down on drop,
        // so a failing assertion below cannot leak it into the rest of the suite.
        let _listeners = GlobalListeners::install(&state);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <SettingsPanel /> }
        });
        next_tick().await;
        click_tab(&container, "Overview");
        next_tick().await;

        let delete = find_button_by_text(&container, "Delete").expect("Delete button");
        delete.focus().unwrap();
        delete.click();
        for _ in 0..3 {
            next_tick().await;
        }
        let dialog = confirm_dialog(&container).expect("the confirmation must open");
        let cancel = find_button_by_text(&dialog, "Cancel").expect("cancel");
        assert!(
            is_focused(&cancel),
            "focus must be inside the dialog before Escape, or this proves nothing"
        );

        // Escape, from where focus actually is.
        cancel.dispatch_event(&key_event("Escape", false)).unwrap();
        for _ in 0..3 {
            next_tick().await;
        }

        assert!(
            confirm_dialog(&container).is_none(),
            "Escape must close the confirmation"
        );
        // …and must close nothing else.
        assert!(
            state.settings_open.get_untracked(),
            "Escape inside the modal must not also tear down the Settings overlay"
        );
        // Focus restoration can only work because the opener was never unmounted.
        assert!(
            is_focused(&delete),
            "focus must return to the Delete button, not fall to <body>. Focus is on: {:?}",
            focused_label()
        );
        assert!(
            last_launch_profiles(&calls).is_none(),
            "Escape must cancel the delete, never persist it"
        );

        // And the global handler is still the global handler. With no modal open,
        // Escape closes the Settings overlay exactly as it did before — the fix
        // narrows the global handler, it does not disable it.
        delete.dispatch_event(&key_event("Escape", false)).unwrap();
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            !state.settings_open.get_untracked(),
            "with no modal open, Escape must still close the Settings overlay"
        );
    }

    /// Only a typed native-settings target may fail a pending native save.
    ///
    /// It closes the bug where `fail_native_settings_pending_on_error` matched on
    /// `request_kind == SetSetting` alone: *any* failed host setting arriving while a
    /// save was in flight would have failed that save and printed the other command's
    /// error on the settings page — against a save the server never even refused,
    /// unlocking controls on a false report.
    ///
    /// Every negative runs first and the real rejection last, so a pipeline that was
    /// simply broken end to end could not let the negatives pass.
    #[wasm_bindgen_test]
    async fn only_a_typed_native_settings_target_fails_the_pending_save() {
        const HOST_B: &str = "host-tyc-other";
        let container = make_container();
        let state = AppState::new();
        crate::dispatch::prime_host_for_tests(&state, TYCODE_HOST);
        crate::dispatch::prime_host_for_tests(&state, HOST_B);
        install_tycode_native_host(&state, tycode_ready_snapshot());
        mark_native_save_pending(&state, TYCODE_HOST);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_some(),
            "the page must start in the in-flight saving state, or this proves nothing"
        );

        // Everything that is *not* this save's answer. Each must leave it in flight.
        let not_our_answer: [(&str, FrameKind, Option<HostSettingErrorTarget>, &str); 5] = [
            (
                "a legacy backend-config setting",
                FrameKind::SetSetting,
                Some(HostSettingErrorTarget::BackendConfig),
                "backend config rejected: unknown key",
            ),
            (
                "an ordinary host setting",
                FrameKind::SetSetting,
                Some(HostSettingErrorTarget::LaunchProfiles),
                "launch profile id is reserved",
            ),
            (
                "a malformed SetSetting payload, which denotes no setting at all",
                FrameKind::SetSetting,
                Some(HostSettingErrorTarget::Malformed),
                "host setting payload was not valid",
            ),
            (
                "an older host that sends no target, so cannot correlate at all",
                FrameKind::SetSetting,
                None,
                "set_setting failed for some reason we cannot attribute",
            ),
            (
                "a command that is not a SetSetting",
                FrameKind::ListSessions,
                None,
                "list_sessions failed",
            ),
        ];

        let mut seq = 0;
        for (what, request_kind, setting_target, message) in not_our_answer {
            dispatch_command_error(
                &state,
                TYCODE_HOST,
                seq,
                request_kind,
                setting_target,
                CommandErrorCode::Internal,
                message,
            );
            seq += 1;
            for _ in 0..3 {
                next_tick().await;
            }
            assert!(
                matches!(
                    native_save_state(&state, TYCODE_HOST),
                    Some(NativeSettingsSaveState::Pending { .. })
                ),
                "{what} must leave the in-flight save waiting for its own answer, not \
                 fail it: {:?}",
                native_save_state(&state, TYCODE_HOST)
            );
            assert!(
                container
                    .query_selector(".settings-native-saving")
                    .unwrap()
                    .is_some(),
                "{what} must not unlock the controls of a save the server never refused"
            );
            let page = container.text_content().unwrap_or_default();
            assert!(
                !page.contains(message),
                "{what} must not put its message on the settings page: {page:?}"
            );
        }

        // A real native-save rejection \u{2014} for a *different host*. Not this save's.
        dispatch_command_error(
            &state,
            HOST_B,
            0,
            FrameKind::SetSetting,
            Some(HostSettingErrorTarget::BackendNativeSettings),
            CommandErrorCode::Internal,
            "another host's save failed",
        );
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            matches!(
                native_save_state(&state, TYCODE_HOST),
                Some(NativeSettingsSaveState::Pending { .. })
            ),
            "a save rejected on another host must not fail this host's save"
        );

        // …and finally the one shape that *is* this save's answer.
        const REJECTION: &str = "Tycode SaveSettings rejected: provider unavailable";
        dispatch_command_error(
            &state,
            TYCODE_HOST,
            seq,
            FrameKind::SetSetting,
            Some(HostSettingErrorTarget::BackendNativeSettings),
            CommandErrorCode::Internal,
            REJECTION,
        );
        for _ in 0..3 {
            next_tick().await;
        }
        assert_eq!(
            native_save_state(&state, TYCODE_HOST),
            Some(NativeSettingsSaveState::Failed {
                message: REJECTION.to_owned(),
            }),
            "the typed native-settings target, on this host, is this save's answer"
        );
        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_none(),
            "the matching rejection must release the save gate \u{2014} no snapshot will"
        );
        let error = container
            .query_selector(".settings-native-error")
            .unwrap()
            .expect("the matching rejection must surface an error on the settings page")
            .text_content()
            .unwrap_or_default();
        assert!(
            error.contains(REJECTION),
            "the server's reason, verbatim: {error:?}"
        );
    }

    /// The destructive confirmation is a real modal: announced, focused, trapped,
    /// and escapable — and Escape cancels rather than confirming. The dialog is
    /// shared, so the contract has to hold for every caller.
    #[wasm_bindgen_test]
    async fn launch_profile_delete_confirmation_is_an_accessible_modal() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_launch_profile_host(
                &state,
                vec![
                    launch_profile_config("hermes:claude", "Hermes \u{b7} Claude"),
                    launch_profile_config("hermes:codex", "Hermes \u{b7} Codex"),
                ],
            );
            provide_context(state);
            view! { <LaunchProfilesSection /> }
        });
        next_tick().await;

        let delete = find_button_by_text(&container, "Delete").expect("Delete button");
        delete.focus().unwrap();
        delete.click();
        for _ in 0..3 {
            next_tick().await;
        }

        let (cancel, confirm) = assert_confirm_dialog_is_an_accessible_modal(
            &container,
            "Delete launch profile",
            "launch profile delete",
        );
        assert_focus_is_trapped_and_escape_cancels(
            &container,
            &cancel,
            &confirm,
            "launch profile delete",
        )
        .await;

        assert!(
            last_launch_profiles(&calls).is_none(),
            "Escape must cancel the delete, never persist it"
        );
        assert!(
            is_focused(&delete),
            "closing must return focus to the Delete button that opened the dialog. Focus is \
             on: {:?}",
            focused_label()
        );
    }

    /// Absent properties render an explicit unset state (marker + "Not set"
    /// placeholder), never a blank/default control that reads as a real value.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_missing_values_show_unset() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(serde_json::json!({
                        "providers": { "anthropic": { "model": "claude" } }
                    }))),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "anthropic".to_owned(),
                        title: "Anthropic".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Module,
                        settings_path: vec!["providers".to_owned(), "anthropic".to_owned()],
                        description: None,
                        schema: serde_json::json!({
                            "properties": {
                                "model": { "type": "string", "title": "Model" },
                                "region": { "type": "string", "title": "Region" }
                            }
                        }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Unset"),
            "an absent property must be marked unset: {text:?}"
        );
        // The present `model` seeds a real value; the absent `region` is unset.
        let inputs = container
            .query_selector_all("input[type=\"text\"].settings-native-input")
            .unwrap();
        assert_eq!(inputs.length(), 2, "both string fields render");
        // Fields are alphabetical (serde_json map order): model (0), region (1).
        let model: HtmlInputElement = inputs.item(0).unwrap().dyn_into().unwrap();
        let region: HtmlInputElement = inputs.item(1).unwrap().dyn_into().unwrap();
        assert_eq!(
            model.value(),
            "claude",
            "the present value seeds its control"
        );
        assert_eq!(region.value(), "", "the absent value is not invented");
        assert_eq!(
            region.get_attribute("placeholder").as_deref(),
            Some("Not set"),
            "the absent field is explicitly marked Not set"
        );
    }

    /// A group whose settings_path is absent from the document states that
    /// explicitly instead of showing invented empty values.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_absent_group_path_shows_note() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(
                        serde_json::json!({ "active_provider": "anthropic" }),
                    )),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "anthropic".to_owned(),
                        title: "Anthropic".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Module,
                        settings_path: vec!["providers".to_owned(), "anthropic".to_owned()],
                        description: None,
                        schema: serde_json::json!({
                            "properties": { "model": { "type": "string" } }
                        }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let note = container
            .query_selector(".settings-native-unset-note")
            .unwrap()
            .expect("an absent group path must be stated explicitly");
        assert!(
            note.text_content()
                .unwrap_or_default()
                .contains("not present"),
            "the note must say the settings are not present"
        );
    }

    /// Nullable JSON-schema type arrays (`["string", "null"]`, `["boolean",
    /// "null"]`) still render typed controls rather than a raw JSON editor.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_nullable_type_renders_typed_controls() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(serde_json::json!({
                        "endpoint": "https://api.example",
                        "verbose": true
                    }))),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "core".to_owned(),
                        title: "Core".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Core,
                        settings_path: Vec::new(),
                        description: None,
                        schema: serde_json::json!({
                            "properties": {
                                "endpoint": { "type": ["string", "null"], "title": "Endpoint" },
                                "verbose": { "type": ["boolean", "null"], "title": "Verbose" }
                            }
                        }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // No raw JSON editor — nullable primitives resolve to typed controls.
        assert!(
            container
                .query_selector("textarea.settings-native-json-input")
                .unwrap()
                .is_none(),
            "nullable primitive types must not fall through to JSON editing"
        );
        let endpoint: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("a nullable string renders a text control")
            .dyn_into()
            .unwrap();
        assert_eq!(endpoint.value(), "https://api.example");
        let checkbox: HtmlInputElement = container
            .query_selector("input[type=\"checkbox\"]")
            .unwrap()
            .expect("a nullable boolean renders a checkbox")
            .dyn_into()
            .unwrap();
        assert!(
            checkbox.checked(),
            "the nullable boolean seeds from its current value"
        );
    }

    /// A server-side rejection of a native save (a `CommandError` for
    /// `SetSetting` after the save reached the server) must unlock the controls
    /// and surface the server's error. The save's result otherwise only lands via
    /// a refreshed native snapshot, and a rejection emits none — so without this
    /// the page stays stuck in "Saving…" forever.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_server_rejection_unlocks_and_shows_error() {
        let container = make_container();
        let state = AppState::new();
        // Prime the inbound validators first (this dispatches a synthetic
        // HostBootstrap, which clears native state for the host), then install the
        // snapshot and mark a save in flight.
        crate::dispatch::prime_host_for_tests(&state, "host-tyc-native");
        install_tycode_native_host(&state, tycode_ready_snapshot());
        let base = state
            .backend_native_settings
            .get_untracked()
            .get("host-tyc-native")
            .and_then(|m| m.get(&BackendKind::Tycode))
            .and_then(|s| s.settings.clone())
            .expect("installed snapshot settings");
        state.native_settings_save_state.update(|m| {
            m.entry("host-tyc-native".to_owned()).or_default().insert(
                BackendKind::Tycode,
                NativeSettingsSaveState::Pending { base },
            );
        });

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_some(),
            "the page starts in the in-flight saving state"
        );

        // The server rejects the SetSetting (e.g. Tycode SaveSettings fails). No
        // refreshed snapshot follows, so only this CommandError can release the
        // gate.
        let env = protocol::Envelope::from_payload(
            protocol::StreamPath("/host/host-tyc-native".to_owned()),
            protocol::FrameKind::CommandError,
            0,
            &protocol::CommandErrorPayload {
                stream: protocol::StreamPath("/host/host-tyc-native".to_owned()),
                request_kind: protocol::FrameKind::SetSetting,
                // The typed target the server now sends for a rejected native save.
                // It is what tells the dispatcher this is the save's answer — and,
                // just as importantly, that it is *not* a managed-projection reset's.
                setting_target: Some(HostSettingErrorTarget::BackendNativeSettings),
                operation: "set_setting".to_owned(),
                code: protocol::CommandErrorCode::Internal,
                message: "Tycode SaveSettings rejected: provider unavailable".to_owned(),
                fatal: false,
            },
        )
        .expect("synthetic CommandError");
        crate::dispatch::dispatch_envelope(&state, "host-tyc-native", env);
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_none(),
            "a server rejection must clear the saving state (no snapshot will)"
        );
        let error = container
            .query_selector(".settings-native-error")
            .unwrap()
            .expect("the server rejection must surface an error on the settings page");
        assert!(
            error
                .text_content()
                .unwrap_or_default()
                .contains("provider unavailable"),
            "the server's error message must be shown"
        );
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must render")
            .dyn_into()
            .unwrap();
        assert!(
            !select.disabled(),
            "controls must unlock after a server rejection so the user can retry"
        );
    }

    /// An explicit JSON `null` from the server is unset, not a concrete value:
    /// a nullable boolean shows unchecked-and-marked-unset (not a real `false`),
    /// an enum shows "Not set" (not a real option), and a string shows empty with
    /// a "Not set" hint. Non-null siblings still render their real value.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_explicit_null_renders_unset() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(serde_json::json!({
                        "active_provider": null,
                        "endpoint": null,
                        "verbose": null,
                        "model": "claude"
                    }))),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "core".to_owned(),
                        title: "Core".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Core,
                        settings_path: Vec::new(),
                        description: None,
                        schema: serde_json::json!({
                            "properties": {
                                "active_provider": {
                                    "type": ["string", "null"],
                                    "enum": ["anthropic", "bedrock"]
                                },
                                "endpoint": { "type": ["string", "null"] },
                                "verbose": { "type": ["boolean", "null"] },
                                "model": { "type": "string" }
                            }
                        }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // Three null fields are marked unset; the non-null `model` is not.
        assert_eq!(
            container
                .query_selector_all(".settings-native-unset")
                .unwrap()
                .length(),
            3,
            "each explicit-null field is marked unset; the non-null one is not"
        );

        // Enum null → "Not set" (empty), not a real option.
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must render")
            .dyn_into()
            .unwrap();
        assert_eq!(
            select.value(),
            "",
            "an explicit-null enum shows Not set, not a concrete option"
        );

        // Boolean null → unchecked AND marked unset, never a concrete false.
        let checkbox: HtmlInputElement = container
            .query_selector("input[type=\"checkbox\"]")
            .unwrap()
            .expect("a nullable boolean renders a checkbox")
            .dyn_into()
            .unwrap();
        assert!(
            !checkbox.checked(),
            "an explicit-null boolean must not render as a concrete checked value"
        );

        // Field order is alphabetical: endpoint (0), model (1).
        let text_inputs = container
            .query_selector_all("input[type=\"text\"].settings-native-input")
            .unwrap();
        let endpoint: HtmlInputElement = text_inputs.item(0).unwrap().dyn_into().unwrap();
        assert_eq!(endpoint.value(), "", "an explicit-null string is empty");
        assert_eq!(
            endpoint.get_attribute("placeholder").as_deref(),
            Some("Not set"),
            "an explicit-null string is marked Not set, not a blank default"
        );
        let model: HtmlInputElement = text_inputs.item(1).unwrap().dyn_into().unwrap();
        assert_eq!(
            model.value(),
            "claude",
            "a non-null sibling still renders its real value"
        );
    }

    /// An accepted no-op edit (re-entering the current value) neither sends a
    /// save nor locks the page — otherwise it would strand the page in
    /// "Saving…" waiting for a change that never happened.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_noop_save_does_not_lock() {
        let calls = install_settings_send_stub();
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // Re-enter the current value: `model` is already "claude".
        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the model control must render")
            .dyn_into()
            .unwrap();
        set_and_change(&model, "claude");
        for _ in 0..3 {
            next_tick().await;
        }

        assert!(
            last_native_settings(&calls).is_none(),
            "a no-op edit must not send a BackendNativeSettings save"
        );
        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_none(),
            "a no-op edit must not lock the page in a saving state"
        );
        assert!(
            !model.disabled(),
            "controls must remain editable after a no-op edit"
        );
    }

    /// The server force-emits a native-settings snapshot after every save, even
    /// when the saved document is unchanged (an accepted no-op or a
    /// canonicalize-to-current). Receiving that snapshot must clear the pending
    /// gate and unlock the page even though the settings value equals the base
    /// the save started from.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_forced_snapshot_clears_saving_when_unchanged() {
        let _calls = install_settings_send_stub();
        let container = make_container();
        let state = AppState::new();
        // Prime the inbound validators first (this dispatches a synthetic
        // HostBootstrap, which clears native state for the host), then install the
        // snapshot.
        crate::dispatch::prime_host_for_tests(&state, "host-tyc-native");
        install_tycode_native_host(&state, tycode_ready_snapshot());
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // A real edit puts a save in flight and locks the controls.
        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the model control must render")
            .dyn_into()
            .unwrap();
        set_and_change(&model, "opus");
        next_tick().await;
        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_some(),
            "the edit must put a save in flight"
        );

        // The server accepted the save but the resulting document is unchanged
        // (canonicalized back to the base), and force-emits the snapshot anyway.
        // The frame carries settings equal to the base the save started from.
        let env = protocol::Envelope::from_payload(
            protocol::StreamPath("/host/host-tyc-native".to_owned()),
            protocol::FrameKind::BackendConfigSnapshots,
            0,
            &protocol::BackendConfigSnapshotsPayload {
                snapshots: Vec::new(),
                native_settings: vec![tycode_ready_snapshot()],
            },
        )
        .expect("synthetic BackendConfigSnapshots");
        crate::dispatch::dispatch_envelope(&state, "host-tyc-native", env);
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_none(),
            "an unchanged force-emitted snapshot must clear the saving gate"
        );
        let select: HtmlSelectElement = container
            .query_selector(".settings-native-group select")
            .unwrap()
            .expect("the enum control must render")
            .dyn_into()
            .unwrap();
        assert!(
            !select.disabled(),
            "controls must unlock once the server confirms, even when unchanged"
        );
    }

    // ---- Native settings: grouped tab strip ----

    /// Native-settings tab labels in DOM order.
    fn native_tab_labels(container: &HtmlElement) -> Vec<String> {
        // Scoped to the group strip: Tycode renders a separate profile-chip
        // tablist above it, which is not part of this contract.
        let tabs = container
            .query_selector_all(".settings-native-tabs [role=\"tab\"]")
            .unwrap();
        (0..tabs.length())
            .map(|i| tabs.item(i).unwrap().text_content().unwrap_or_default())
            .collect()
    }

    /// The one native-settings tab whose label contains `needle`.
    fn native_tab_by_label(container: &HtmlElement, needle: &str) -> HtmlElement {
        let tabs = container
            .query_selector_all(".settings-native-tabs [role=\"tab\"]")
            .unwrap();
        for i in 0..tabs.length() {
            let el: HtmlElement = tabs.item(i).unwrap().dyn_into().unwrap();
            if el.text_content().unwrap_or_default().contains(needle) {
                return el;
            }
        }
        panic!("no native settings tab labelled {needle:?}");
    }

    /// The group panels in DOM order (Core first).
    fn native_panels(container: &HtmlElement) -> Vec<HtmlElement> {
        let panels = container
            .query_selector_all(".settings-native-group-panel")
            .unwrap();
        (0..panels.length())
            .map(|i| panels.item(i).unwrap().dyn_into().unwrap())
            .collect()
    }

    /// A backend whose native settings span several groups renders them as a tab
    /// strip (one tab per group, Core first) with only the active group's panel
    /// visible — never one long flat form.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_render_group_tabs() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let labels = native_tab_labels(&container);
        assert_eq!(
            labels.len(),
            2,
            "one tab per server-provided group: {labels:?}"
        );
        assert!(
            labels[0].contains("Core"),
            "the Core group anchors the strip: {labels:?}"
        );
        assert!(
            labels[1].contains("Anthropic"),
            "module groups follow Core: {labels:?}"
        );

        let panels = native_panels(&container);
        assert_eq!(panels.len(), 2, "one panel per group");
        assert!(!panels[0].hidden(), "the Core panel is visible by default");
        assert!(
            panels[1].hidden(),
            "only the active group's fields show; the module panel is hidden"
        );
    }

    /// Clicking a module tab reveals its fields and hides the previously active
    /// group — the module's controls (mounted but hidden) become visible.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_tab_click_switches_panel() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(&state, tycode_ready_snapshot());
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let anthropic_tab = native_tab_by_label(&container, "Anthropic");
        anthropic_tab.click();
        for _ in 0..3 {
            next_tick().await;
        }

        let panels = native_panels(&container);
        assert!(
            panels[0].hidden(),
            "the Core panel hides once a module tab is active"
        );
        assert!(
            !panels[1].hidden(),
            "the clicked module panel becomes visible"
        );
        assert_eq!(
            anthropic_tab.get_attribute("aria-selected").as_deref(),
            Some("true"),
            "the active tab reports itself selected"
        );
    }

    /// The Core group is the leftmost tab even when the server lists module
    /// groups ahead of it, so the anchor page is always first.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_core_group_ordered_first() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            let mut snapshot = tycode_ready_snapshot();
            snapshot.groups.reverse(); // Module now precedes Core in server order.
            install_tycode_native_host(&state, snapshot);
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        let labels = native_tab_labels(&container);
        assert!(
            labels[0].contains("Core"),
            "Core anchors the strip regardless of server order: {labels:?}"
        );
        let panels = native_panels(&container);
        assert!(
            !panels[0].hidden(),
            "the Core panel is the default-visible one"
        );
    }

    /// A backend that exposes exactly one native group renders it directly with
    /// its titled header and no tab strip.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_single_group_has_no_tabs() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            install_tycode_native_host(
                &state,
                BackendNativeSettingsSnapshot {
                    backend_kind: BackendKind::Tycode,
                    status: BackendConfigSnapshotStatus::Ready,
                    settings: Some(tycode_profiles_doc(
                        serde_json::json!({ "active_provider": "anthropic" }),
                    )),
                    groups: vec![BackendNativeSettingsGroup {
                        id: "core".to_owned(),
                        title: "Core".to_owned(),
                        kind: BackendNativeSettingsGroupKind::Core,
                        settings_path: Vec::new(),
                        description: None,
                        schema: serde_json::json!({
                            "properties": {
                                "active_provider": {
                                    "type": "string",
                                    "title": "Active Provider"
                                }
                            }
                        }),
                    }],
                    message: None,
                    advisories: Vec::new(),
                },
            );
            provide_context(state);
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // Scoped to the group strip: the profile-chip tablist above it is a
        // separate control and renders regardless of the group count.
        assert!(
            container
                .query_selector(".settings-native-tabs [role=\"tab\"]")
                .unwrap()
                .is_none(),
            "a single native group must not grow a tab strip"
        );
        assert!(
            container
                .query_selector(".settings-native-group-header")
                .unwrap()
                .is_some(),
            "the single group keeps its titled header"
        );
    }

    /// Regression: the selected module tab must survive the native body being
    /// rebuilt by a save (which flips `native_settings_save_state` to Pending)
    /// and by the forced snapshot the server emits afterward — the user is never
    /// snapped back to the Core tab mid-edit.
    #[wasm_bindgen_test]
    async fn tycode_native_settings_active_tab_survives_save_and_snapshot() {
        let _calls = install_settings_send_stub();
        let container = make_container();
        let state = AppState::new();
        // Prime validators so the forced BackendConfigSnapshots dispatch is
        // accepted later.
        crate::dispatch::prime_host_for_tests(&state, "host-tyc-native");
        install_tycode_native_host(&state, tycode_ready_snapshot());
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <BackendSettingsPage kind=BackendKind::Tycode /> }
        });
        next_tick().await;

        // Switch to the Anthropic module tab.
        let anthropic_tab = native_tab_by_label(&container, "Anthropic");
        anthropic_tab.click();
        for _ in 0..3 {
            next_tick().await;
        }
        assert!(
            !native_panels(&container)[1].hidden(),
            "the module panel is active before editing"
        );

        // Edit the module's model field → a save goes in flight (Pending), which
        // rebuilds the native body.
        let model: HtmlInputElement = container
            .query_selector("input[type=\"text\"].settings-native-input")
            .unwrap()
            .expect("the model control renders in the active module panel")
            .dyn_into()
            .unwrap();
        set_and_change(&model, "opus");
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_some(),
            "the edit must put a save in flight"
        );
        let panels = native_panels(&container);
        assert!(
            panels[0].hidden(),
            "Core stays hidden while a save is in flight"
        );
        assert!(
            !panels[1].hidden(),
            "the module tab stays active through the save rerender, not reset to Core"
        );

        // The server force-emits a fresh snapshot; the body rebuilds again.
        let env = protocol::Envelope::from_payload(
            protocol::StreamPath("/host/host-tyc-native".to_owned()),
            protocol::FrameKind::BackendConfigSnapshots,
            0,
            &protocol::BackendConfigSnapshotsPayload {
                snapshots: Vec::new(),
                native_settings: vec![tycode_ready_snapshot()],
            },
        )
        .expect("synthetic BackendConfigSnapshots");
        crate::dispatch::dispatch_envelope(&state, "host-tyc-native", env);
        next_tick().await;

        assert!(
            container
                .query_selector(".settings-native-saving")
                .unwrap()
                .is_none(),
            "the forced snapshot clears the saving gate"
        );
        let panels = native_panels(&container);
        assert!(
            panels[0].hidden(),
            "Core is still hidden after the snapshot rebuild"
        );
        assert!(
            !panels[1].hidden(),
            "the module tab remains active after the forced snapshot"
        );
        let anthropic_tab = native_tab_by_label(&container, "Anthropic");
        assert_eq!(
            anthropic_tab.get_attribute("aria-selected").as_deref(),
            Some("true"),
            "the module tab is still marked selected after the snapshot"
        );
    }
}
