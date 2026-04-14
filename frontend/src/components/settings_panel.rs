use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::AppState;

use protocol::{
    BackendKind, DumpSettingsPayload, FrameKind, HostSettingValue, SetSettingPayload,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum SettingsTab {
    Appearance,
    General,
    Backends,
}

#[component]
pub fn SettingsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let active_tab = RwSignal::new(SettingsTab::Appearance);

    let state_for_refresh = state.clone();
    Effect::new(move |_| {
        if state_for_refresh.settings_open.get() {
            request_host_settings(&state_for_refresh);
        }
    });

    let on_close = move |_| {
        state.settings_open.set(false);
    };

    let on_backdrop = move |_| {
        state.settings_open.set(false);
    };

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" {
            ev.prevent_default();
            state.settings_open.set(false);
        }
    };

    view! {
        <Show when=move || state.settings_open.get()>
            <div class="sp-overlay" on:click=on_backdrop on:keydown=on_keydown>
                <div class="sp-panel" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                    <div class="sp-header">
                        <span class="sp-title">"Settings"</span>
                        <button class="sp-close" on:click=on_close title="Close">"×"</button>
                    </div>
                    <div class="sp-tabs">
                        <button
                            class="sp-tab"
                            class:active=move || active_tab.get() == SettingsTab::Appearance
                            on:click=move |_| active_tab.set(SettingsTab::Appearance)
                        >"Appearance"</button>
                        <button
                            class="sp-tab"
                            class:active=move || active_tab.get() == SettingsTab::General
                            on:click=move |_| active_tab.set(SettingsTab::General)
                        >"General"</button>
                        <button
                            class="sp-tab"
                            class:active=move || active_tab.get() == SettingsTab::Backends
                            on:click=move |_| active_tab.set(SettingsTab::Backends)
                        >"Backends"</button>
                    </div>
                    <div class="sp-content">
                        {move || match active_tab.get() {
                            SettingsTab::Appearance => view! { <AppearanceTab /> }.into_any(),
                            SettingsTab::General => view! { <GeneralTab /> }.into_any(),
                            SettingsTab::Backends => view! { <BackendsTab /> }.into_any(),
                        }}
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
fn AppearanceTab() -> impl IntoView {
    let state = expect_context::<AppState>();

    let on_font_size = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlInputElement = target.unchecked_into();
        if let Ok(v) = el.value().parse::<u32>() {
            state.font_size.set(v);
            if let Some(doc) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
            {
                let style: web_sys::HtmlElement = doc.unchecked_into();
                let _ = style.style().set_property("--base-font-size", &format!("{v}px"));
            }
        }
    };

    view! {
        <div class="sp-section">
            <h3 class="sp-section-title">"Theme"</h3>
            <div class="sp-row">
                <span class="sp-label">"Color theme"</span>
                <div class="sp-toggle-group">
                    <button class="sp-toggle active">"Dark"</button>
                    <button class="sp-toggle" disabled=true title="Not yet available">"Light"</button>
                    <button class="sp-toggle" disabled=true title="Not yet available">"System"</button>
                </div>
            </div>
        </div>
        <div class="sp-section">
            <h3 class="sp-section-title">"Font"</h3>
            <div class="sp-row">
                <span class="sp-label">"Font size"</span>
                <div class="sp-slider-group">
                    <input
                        type="range"
                        class="sp-slider"
                        min="11"
                        max="20"
                        prop:value=move || state.font_size.get().to_string()
                        on:input=on_font_size
                    />
                    <span class="sp-slider-value">{move || format!("{}px", state.font_size.get())}</span>
                </div>
            </div>
            <div class="sp-row">
                <span class="sp-label">"Font family"</span>
                <select class="sp-select" disabled=true>
                    <option>"System"</option>
                    <option>"Monospace"</option>
                </select>
            </div>
        </div>
    }
}

#[component]
fn GeneralTab() -> impl IntoView {
    view! {
        <div class="sp-section">
            <h3 class="sp-section-title">"Connection"</h3>
            <div class="sp-row">
                <span class="sp-label">"Auto-connect on launch"</span>
                <label class="sp-checkbox-label">
                    <input type="checkbox" class="sp-checkbox" checked=true disabled=true />
                    <span>"Enabled"</span>
                </label>
            </div>
        </div>
    }
}

#[component]
fn BackendsTab() -> impl IntoView {
    let state = expect_context::<AppState>();

    let default_backend_value = move || {
        state
            .host_settings
            .get()
            .map(|settings| backend_value(settings.default_backend).to_owned())
            .unwrap_or_default()
    };

    view! {
        <div class="sp-section">
            <h3 class="sp-section-title">"Backend Settings"</h3>
            {move || match state.host_settings.get() {
                Some(settings) => {
                    let state_for_default_backend_change = state.clone();
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
                        <div class="sp-row">
                            <span class="sp-label">"Default backend"</span>
                            <select
                                class="sp-select"
                                prop:value=default_backend_value
                                on:change=move |ev: web_sys::Event| {
                                    let target = ev.target().unwrap();
                                    let el: web_sys::HtmlSelectElement = target.unchecked_into();
                                    let Some(default_backend) = parse_backend_kind(&el.value()) else {
                                        log::error!("unknown backend value {}", el.value());
                                        return;
                                    };
                                    send_host_setting(
                                        &state_for_default_backend_change,
                                        HostSettingValue::DefaultBackend { default_backend },
                                    );
                                }
                            >
                                {options}
                            </select>
                        </div>
                    }
                    .into_any()
                }
                None => view! { <div class="panel-empty">"Host settings not loaded"</div> }.into_any(),
            }}
            <div class="sp-backend-list">
                {all_backends()
                    .into_iter()
                    .map(|kind| view! { <BackendCard kind /> })
                    .collect::<Vec<_>>()}
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

    let checked = move || {
        state
            .host_settings
            .get()
            .is_some_and(|settings| settings.enabled_backends.contains(&kind))
    };
    let disable_toggle = move || {
        state.host_settings.get().is_none_or(|settings| {
            settings.enabled_backends.len() == 1 && settings.enabled_backends.contains(&kind)
        })
    };
    let status_class = move || {
        if checked() {
            "sp-status-badge enabled"
        } else {
            "sp-status-badge disabled"
        }
    };
    let status_text = move || {
        if checked() { "Enabled" } else { "Disabled" }
    };

    let on_toggle = {
        let state = state.clone();
        move |ev: web_sys::Event| {
            let target = ev.target().unwrap();
            let input: web_sys::HtmlInputElement = target.unchecked_into();
            let Some(settings) = state.host_settings.get_untracked() else {
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
        <div class="sp-backend-card">
            <div class="sp-backend-header">
                <span class=badge_class>{name}</span>
                <span class=status_class>{status_text}</span>
            </div>
            <p class="sp-backend-desc">{description}</p>
            <label class="sp-checkbox-label">
                <input
                    type="checkbox"
                    class="sp-checkbox"
                    prop:checked=checked
                    disabled=disable_toggle
                    on:change=on_toggle
                />
                <span>"Enabled for this host"</span>
            </label>
        </div>
    }
}

fn request_host_settings(state: &AppState) {
    let Some(host_id) = state.host_id.get_untracked() else {
        log::error!("request_host_settings: not connected");
        return;
    };
    let Some(host_stream) = state.host_stream.get_untracked() else {
        log::error!("request_host_settings: no host stream");
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
    let Some(host_id) = state.host_id.get_untracked() else {
        log::error!("send_host_setting: not connected");
        return;
    };
    let Some(host_stream) = state.host_stream.get_untracked() else {
        log::error!("send_host_setting: no host stream");
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

fn all_backends() -> [BackendKind; 3] {
    [BackendKind::Claude, BackendKind::Codex, BackendKind::Gemini]
}

fn parse_backend_kind(value: &str) -> Option<BackendKind> {
    match value {
        "claude" => Some(BackendKind::Claude),
        "codex" => Some(BackendKind::Codex),
        "gemini" => Some(BackendKind::Gemini),
        _ => None,
    }
}

fn backend_value(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Gemini => "gemini",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

fn backend_description(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => "Anthropic Claude — advanced reasoning and coding",
        BackendKind::Codex => "OpenAI Codex — code completion and generation",
        BackendKind::Gemini => "Google Gemini — multimodal AI assistant",
    }
}

fn backend_badge_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Gemini => "backend-badge gemini",
    }
}
