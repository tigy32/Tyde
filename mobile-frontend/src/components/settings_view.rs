use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::host_browser::HostBrowser;
use crate::components::ui::{Button, ButtonSize, ButtonVariant, EmptyState};
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

#[cfg(test)]
mod tool_output_mode_tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            assert_eq!(
                tool_output_mode_from_str(tool_output_mode_to_str(mode)),
                Some(mode)
            );
        }
    }

    #[test]
    fn unknown_is_none() {
        assert_eq!(tool_output_mode_from_str(""), None);
        assert_eq!(tool_output_mode_from_str("bogus"), None);
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
            <div class="view-body">
                <div class="settings-section" data-mobile-test="settings-appearance">
                    <h2 class="settings-section-title">"Appearance"</h2>
                    <div class="settings-row">
                        <span class="settings-label">"Theme"</span>
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
                    </div>
                    <div class="settings-row">
                        <span class="settings-label">"Tool Output"</span>
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
                    </div>
                </div>

                <PairedHostSection />

                <div class="settings-section">
                    <h2 class="settings-section-title">"Host"</h2>
                    {let state = state.clone(); move || {
                        let settings = state.active_host_settings();
                        match settings {
                            Some(hs) => {
                                let backends: Vec<String> = hs.enabled_backends.iter().map(|b| format!("{b:?}")).collect();
                                let default = hs.default_backend.map(|b| format!("{b:?}")).unwrap_or_else(|| "None".to_string());
                                view! {
                                    <div class="settings-info">
                                        <div class="settings-row">
                                            <span class="settings-label">"Enabled Backends"</span>
                                            <span class="settings-value">{backends.join(", ")}</span>
                                        </div>
                                        <div class="settings-row">
                                            <span class="settings-label">"Default Backend"</span>
                                            <span class="settings-value">{default}</span>
                                        </div>
                                    </div>
                                }.into_any()
                            }
                            None => {
                                view! {
                                    <div class="settings-info">
                                        <span class="settings-muted">"Not connected to a host"</span>
                                    </div>
                                }.into_any()
                            }
                        }
                    }}
                </div>

                <div class="settings-section">
                    <h2 class="settings-section-title">"Backend Setup"</h2>
                    {let state = state.clone(); move || {
                        let setup = state.active_host_backend_setup();
                        if setup.is_empty() {
                            return view! {
                                <div class="settings-info">
                                    <span class="settings-muted">"No backend setup info"</span>
                                </div>
                            }.into_any();
                        }
                        view! {
                            <div class="backend-setup-list">
                                {setup.into_iter().map(|info| {
                                    let backend = format!("{:?}", info.backend_kind);
                                    let is_installed = matches!(info.status, protocol::BackendSetupStatus::Installed);
                                    let status_text = format!("{:?}", info.status);
                                    view! {
                                        <div class="backend-setup-card">
                                            <div class="backend-name">{backend}</div>
                                            <div class="backend-status" class:ready=is_installed>
                                                {status_text}
                                            </div>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }}
                </div>

                <div class="settings-section" data-mobile-test="settings-custom-agents">
                    <h2 class="settings-section-title">"Custom Agents"</h2>
                    {let state = state.clone(); move || {
                        let agents = state.active_host_custom_agents();
                        if agents.is_empty() {
                            return view! {
                                <div class="settings-info">
                                    <span class="settings-muted" data-mobile-test="settings-custom-agents-empty">"No custom agents configured"</span>
                                </div>
                            }.into_any();
                        }
                        let mut sorted: Vec<_> = agents.into_values().collect();
                        sorted.sort_by(|a, b| a.name.cmp(&b.name));
                        view! {
                            <div class="custom-agent-list" data-mobile-test="settings-custom-agents-list">
                                {sorted.into_iter().map(|agent| {
                                    let name = agent.name.clone();
                                    let desc = agent.description.clone();
                                    view! {
                                        <div class="custom-agent-card" data-mobile-test="settings-custom-agent-row">
                                            <span class="custom-agent-name">{name}</span>
                                            <span class="custom-agent-backend">{desc}</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }}
                </div>

                <McpServersSection />
                <SteeringSection />
                <SkillsSection />
                <HostToolsSection />

                <div class="settings-section">
                    <h2 class="settings-section-title">"About"</h2>
                    <div class="settings-info">
                        <div class="settings-row">
                            <span class="settings-label">"App"</span>
                            <span class="settings-value">"Tyde Mobile"</span>
                        </div>
                        <div class="settings-row">
                            <span class="settings-label">"Version"</span>
                            <span class="settings-value">"0.1.0"</span>
                        </div>
                    </div>
                </div>
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
        <div class="settings-section">
            <h2 class="settings-section-title">"Paired Host"</h2>
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
        </div>
    }
}

#[component]
fn PairedHostCard(host: PairedHostSummary) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let local_host_id = host.local_host_id.clone();
    let host_label = host.host_label.clone();
    let broker_url = host.broker.url.to_string();
    let room_id = host.room.to_string();
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

    let id_for_forget = local_host_id.clone();
    let state_for_forget = state.clone();
    let on_forget = Callback::new(move |_: ()| {
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
        <div>
            <div class="settings-row">
                <span class="settings-label">"Label"</span>
                <span class="settings-value">{host_label}</span>
            </div>
            <div class="settings-row">
                <span class="settings-label">"Broker"</span>
                <span class="settings-value broker-url" title=broker_url.clone()>{broker_url.clone()}</span>
            </div>
            <div class="settings-row">
                <span class="settings-label">"Room"</span>
                <span class="settings-value room-id">{room_id}</span>
            </div>
            <div class="settings-row">
                <span class="settings-label">"Credential"</span>
                <span class="settings-value credential-fingerprint">{credential_fingerprint}</span>
            </div>
            <div class="settings-row">
                <span class="settings-label">"Last Connected"</span>
                <span class="settings-value">{last_connected}</span>
            </div>
            <div class="settings-row">
                <span class="settings-label">"Auto-connect"</span>
                <input
                    class="settings-toggle"
                    type="checkbox"
                    prop:checked=auto_connect_checked
                    on:change=on_toggle_auto
                />
            </div>
            <div class="settings-row">
                <Button
                    label="Forget host"
                    variant=ButtonVariant::Destructive
                    data_mobile_test="settings-forget-host"
                    aria_label="Forget paired host on this device".to_string()
                    on_click=on_forget
                />
                <p class="settings-hint">
                    "Forget removes the pairing on this device only. To revoke server-side, use Settings → Mobile on the desktop."
                </p>
            </div>
        </div>
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
        <div class="settings-section" data-mobile-test="settings-mcp-servers">
            <h2 class="settings-section-title">"MCP Servers"</h2>
            {move || {
                let Some(host) = state.active_local_host_id.get() else {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted">"Not connected to a host"</span>
                        </div>
                    }.into_any();
                };
                let servers = state
                    .mcp_servers_by_host
                    .with(|m| m.get(&host).cloned())
                    .unwrap_or_default();
                if servers.is_empty() {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted" data-mobile-test="settings-mcp-servers-empty">"No MCP servers configured"</span>
                        </div>
                    }.into_any();
                }
                let mut sorted: Vec<_> = servers.into_values().collect();
                sorted.sort_by(|a, b| a.name.cmp(&b.name));
                view! {
                    <div class="custom-agent-list" data-mobile-test="settings-mcp-servers-list">
                        {sorted.into_iter().map(|server| {
                            let name = server.name.clone();
                            let transport = mcp_transport_label(&server.transport);
                            view! {
                                <div class="custom-agent-card" data-mobile-test="settings-mcp-server-row">
                                    <span class="custom-agent-name">{name}</span>
                                    <span class="custom-agent-backend">{transport}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </div>
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
        <div class="settings-section" data-mobile-test="settings-steering">
            <h2 class="settings-section-title">"Steering"</h2>
            {move || {
                let Some(host) = state.active_local_host_id.get() else {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted">"Not connected to a host"</span>
                        </div>
                    }.into_any();
                };
                let docs = state
                    .steering_by_host
                    .with(|m| m.get(&host).cloned())
                    .unwrap_or_default();
                if docs.is_empty() {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted" data-mobile-test="settings-steering-empty">"No steering documents configured"</span>
                        </div>
                    }.into_any();
                }
                let mut sorted: Vec<_> = docs.into_values().collect();
                sorted.sort_by(|a, b| a.title.cmp(&b.title));
                view! {
                    <div class="custom-agent-list" data-mobile-test="settings-steering-list">
                        {sorted.into_iter().map(|doc| {
                            let title = doc.title.clone();
                            let scope_label = match doc.scope {
                                protocol::SteeringScope::Host => "Host-wide".to_string(),
                                protocol::SteeringScope::Project(pid) => format!("Project: {}", pid.0),
                            };
                            view! {
                                <div class="custom-agent-card" data-mobile-test="settings-steering-row">
                                    <span class="custom-agent-name">{title}</span>
                                    <span class="custom-agent-backend">{scope_label}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

/// Lists every skill available on the active host. Mostly informational
/// for v1; users can spawn a chat that uses them via custom agents.
#[component]
fn SkillsSection() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <div class="settings-section" data-mobile-test="settings-skills">
            <h2 class="settings-section-title">"Skills"</h2>
            {move || {
                let Some(host) = state.active_local_host_id.get() else {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted">"Not connected to a host"</span>
                        </div>
                    }.into_any();
                };
                let skills = state
                    .skills_by_host
                    .with(|m| m.get(&host).cloned())
                    .unwrap_or_default();
                if skills.is_empty() {
                    return view! {
                        <div class="settings-info">
                            <span class="settings-muted" data-mobile-test="settings-skills-empty">"No skills installed"</span>
                        </div>
                    }.into_any();
                }
                let mut sorted: Vec<_> = skills.into_values().collect();
                sorted.sort_by(|a, b| a.name.cmp(&b.name));
                view! {
                    <div class="custom-agent-list" data-mobile-test="settings-skills-list">
                        {sorted.into_iter().map(|skill| {
                            let display = skill.title.clone().unwrap_or_else(|| skill.name.clone());
                            let subtitle = skill.description.clone().unwrap_or_else(|| skill.name.clone());
                            view! {
                                <div class="custom-agent-card" data-mobile-test="settings-skill-row">
                                    <span class="custom-agent-name">{display}</span>
                                    <span class="custom-agent-backend">{subtitle}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </div>
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
        <div class="settings-section" data-mobile-test="settings-host-tools">
            <h2 class="settings-section-title">"Host tools"</h2>
            <div class="settings-info">
                <div class="settings-row">
                    <span class="settings-label">"Browse host filesystem"</span>
                    <Button
                        label="Open"
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Compact
                        data_mobile_test="settings-open-host-browser"
                        on_click=on_open_browser
                    />
                </div>
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
        </div>
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
            // MCP server (http transport).
            let mut mcp = HashMap::new();
            mcp.insert(
                McpServerId("m-1".to_owned()),
                McpServerConfig {
                    id: McpServerId("m-1".to_owned()),
                    name: "search-mcp".to_owned(),
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
}
