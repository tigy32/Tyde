use leptos::prelude::*;
use wasm_bindgen::prelude::*;

use crate::state::AppState;

use protocol::BackendKind;

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
            // Apply to CSS custom property
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
    let state = expect_context::<AppState>();

    let on_backend_change = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlSelectElement = target.unchecked_into();
        let backend = match el.value().as_str() {
            "claude" => BackendKind::Claude,
            "codex" => BackendKind::Codex,
            "gemini" => BackendKind::Gemini,
            _ => return,
        };
        state.default_backend.set(backend);
    };

    let current_backend_value = move || match state.default_backend.get() {
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Gemini => "gemini",
    };

    view! {
        <div class="sp-section">
            <h3 class="sp-section-title">"Default Backend"</h3>
            <div class="sp-row">
                <span class="sp-label">"Agent backend"</span>
                <select
                    class="sp-select"
                    on:change=on_backend_change
                    prop:value=current_backend_value
                >
                    <option value="claude">"Claude"</option>
                    <option value="codex">"Codex"</option>
                    <option value="gemini">"Gemini"</option>
                </select>
            </div>
        </div>
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

#[derive(Clone)]
struct BackendInfo {
    name: &'static str,
    kind: BackendKind,
    description: &'static str,
    enabled: bool,
}

const BACKENDS: &[BackendInfo] = &[
    BackendInfo {
        name: "Claude",
        kind: BackendKind::Claude,
        description: "Anthropic Claude — advanced reasoning and coding",
        enabled: true,
    },
    BackendInfo {
        name: "Codex",
        kind: BackendKind::Codex,
        description: "OpenAI Codex — code completion and generation",
        enabled: true,
    },
    BackendInfo {
        name: "Gemini",
        kind: BackendKind::Gemini,
        description: "Google Gemini — multimodal AI assistant",
        enabled: true,
    },
];

#[component]
fn BackendsTab() -> impl IntoView {
    view! {
        <div class="sp-section">
            <h3 class="sp-section-title">"Available Backends"</h3>
            <div class="sp-backend-list">
                {BACKENDS.iter().map(|b| {
                    let badge_class = match b.kind {
                        BackendKind::Claude => "backend-badge claude",
                        BackendKind::Codex => "backend-badge codex",
                        BackendKind::Gemini => "backend-badge gemini",
                    };
                    let status_class = if b.enabled { "sp-status-badge enabled" } else { "sp-status-badge disabled" };
                    let status_text = if b.enabled { "Enabled" } else { "Disabled" };
                    view! {
                        <div class="sp-backend-card">
                            <div class="sp-backend-header">
                                <span class=badge_class>{b.name}</span>
                                <span class=status_class>{status_text}</span>
                            </div>
                            <p class="sp-backend-desc">{b.description}</p>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
}
