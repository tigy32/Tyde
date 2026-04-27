use leptos::prelude::*;
use wasm_bindgen::JsCast;

use protocol::{
    BackendKind, SessionSchemaEntry, SessionSettingFieldType, SessionSettingValue,
    SessionSettingsSchema, SessionSettingsValues,
};

use crate::state::AppState;

#[component]
pub fn SessionSettingsControls(
    schema: SessionSettingsSchema,
    values: Signal<SessionSettingsValues>,
    on_change: Callback<SessionSettingsValues>,
) -> impl IntoView {
    let fields = schema.fields.clone();

    view! {
        <div class="session-settings">
            {fields.into_iter().map(|field| {
                let key = field.key.clone();
                let label = field.label.clone();
                let description = field.description.clone();
                let field_type = field.field_type.clone();
                let use_slider = field.use_slider;
                let on_change_cb = on_change;

                view! {
                    <div
                        class="session-setting-row"
                        title=move || description.clone().unwrap_or_default()
                    >
                        <span class="session-setting-label">{label}</span>
                        {match field_type {
                            SessionSettingFieldType::Select { options, default, nullable } if use_slider => {
                                // Build ordered entries: optional "Auto" at index 0, then each option.
                                let entries: Vec<(String, String)> = {
                                    let mut v = Vec::new();
                                    if nullable {
                                        v.push((String::new(), "Auto".to_string()));
                                    }
                                    for opt in &options {
                                        v.push((opt.value.clone(), opt.label.clone()));
                                    }
                                    v
                                };
                                let entries_for_read = entries.clone();
                                let entries_for_change = entries.clone();
                                let max_idx = (entries.len().saturating_sub(1)) as i64;

                                let current_idx = {
                                    let key = key.clone();
                                    let default = default.clone();
                                    let entries = entries_for_read.clone();
                                    Signal::derive(move || {
                                        let vals = values.get();
                                        let current_val = match vals.0.get(&key) {
                                            Some(SessionSettingValue::String(s)) => s.clone(),
                                            Some(SessionSettingValue::Null) | None => {
                                                if nullable {
                                                    String::new()
                                                } else {
                                                    default.clone().unwrap_or_default()
                                                }
                                            }
                                            _ => String::new(),
                                        };
                                        entries.iter().position(|(v, _)| v == &current_val)
                                            .unwrap_or(0) as i64
                                    })
                                };

                                let current_label = {
                                    let entries = entries_for_read.clone();
                                    move || {
                                        let idx = current_idx.get() as usize;
                                        entries.get(idx).map(|(_, l)| l.clone()).unwrap_or_default()
                                    }
                                };

                                let on_slider_change = {
                                    let key = key.clone();
                                    move |ev: leptos::ev::Event| {
                                        let text = event_target_value(&ev);
                                        if let Ok(idx) = text.parse::<usize>() {
                                            let mut current = values.get_untracked();
                                            if let Some((val, _)) = entries_for_change.get(idx) {
                                                if val.is_empty() {
                                                    current.0.insert(key.clone(), SessionSettingValue::Null);
                                                } else {
                                                    current.0.insert(key.clone(), SessionSettingValue::String(val.clone()));
                                                }
                                                on_change_cb.run(current);
                                            }
                                        }
                                    }
                                };

                                view! {
                                    <div class="session-setting-slider-wrap">
                                        <input
                                            type="range"
                                            class="session-setting-slider"
                                            min="0"
                                            max=max_idx.to_string()
                                            step="1"
                                            prop:value=move || current_idx.get().to_string()
                                            on:input=on_slider_change
                                        />
                                        <span class="session-setting-value-label">{current_label}</span>
                                    </div>
                                }.into_any()
                            }
                            SessionSettingFieldType::Select { options, default, nullable } => {
                                let key = key.clone();
                                let options_for_view = options.clone();
                                let default_for_view = default.clone();

                                let current_value = {
                                    let key = key.clone();
                                    let default = default_for_view.clone();
                                    move || {
                                        let vals = values.get();
                                        match vals.0.get(&key) {
                                            Some(SessionSettingValue::String(s)) => s.clone(),
                                            Some(SessionSettingValue::Null) | None => {
                                                if nullable {
                                                    String::new()
                                                } else {
                                                    default.clone().unwrap_or_default()
                                                }
                                            }
                                            _ => String::new(),
                                        }
                                    }
                                };

                                let on_select_change = {
                                    let key = key.clone();
                                    move |ev: leptos::ev::Event| {
                                        let selected = event_target_value(&ev);
                                        let mut current = values.get_untracked();
                                        if selected.is_empty() {
                                            current.0.insert(key.clone(), SessionSettingValue::Null);
                                        } else {
                                            current.0.insert(key.clone(), SessionSettingValue::String(selected));
                                        }
                                        on_change_cb.run(current);
                                    }
                                };

                                view! {
                                    <select
                                        class="session-setting-select"
                                        prop:value=current_value
                                        on:change=on_select_change
                                    >
                                        {nullable.then(|| view! {
                                            <option value="">"Auto"</option>
                                        })}
                                        {options_for_view.into_iter().map(|opt| {
                                            view! {
                                                <option value={opt.value.clone()}>{opt.label}</option>
                                            }
                                        }).collect_view()}
                                    </select>
                                }.into_any()
                            }
                            SessionSettingFieldType::Toggle { default } => {
                                let key = key.clone();

                                let current_checked = {
                                    let key = key.clone();
                                    move || {
                                        let vals = values.get();
                                        match vals.0.get(&key) {
                                            Some(SessionSettingValue::Bool(b)) => *b,
                                            _ => default,
                                        }
                                    }
                                };

                                let on_toggle_change = {
                                    let key = key.clone();
                                    move |ev: leptos::ev::Event| {
                                        let target = ev.target().unwrap();
                                        let input: web_sys::HtmlInputElement = target.unchecked_into();
                                        let checked = input.checked();
                                        let mut current = values.get_untracked();
                                        current.0.insert(key.clone(), SessionSettingValue::Bool(checked));
                                        on_change_cb.run(current);
                                    }
                                };

                                view! {
                                    <label class="session-setting-toggle">
                                        <input
                                            type="checkbox"
                                            prop:checked=current_checked
                                            on:change=on_toggle_change
                                        />
                                        <span class="session-setting-toggle-slider"></span>
                                    </label>
                                }.into_any()
                            }
                            SessionSettingFieldType::Integer { min, max, step, default } => {
                                let key = key.clone();

                                let current_int = {
                                    let key = key.clone();
                                    move || {
                                        let vals = values.get();
                                        match vals.0.get(&key) {
                                            Some(SessionSettingValue::Integer(n)) => *n,
                                            _ => default,
                                        }
                                    }
                                };

                                let on_int_change = {
                                    let key = key.clone();
                                    move |ev: leptos::ev::Event| {
                                        let text = event_target_value(&ev);
                                        if let Ok(n) = text.parse::<i64>() {
                                            let clamped = n.clamp(min, max);
                                            let mut current = values.get_untracked();
                                            current.0.insert(key.clone(), SessionSettingValue::Integer(clamped));
                                            on_change_cb.run(current);
                                        }
                                    }
                                };

                                view! {
                                    <input
                                        type="number"
                                        class="session-setting-number"
                                        prop:value=move || current_int().to_string()
                                        on:change=on_int_change
                                        min=min.to_string()
                                        max=max.to_string()
                                        step=step.to_string()
                                        autocomplete="off"
                                    />
                                }.into_any()
                            }
                        }}
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
pub fn SessionSettingsBar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let expanded = RwSignal::new(false);

    let current_binding = {
        let state = state.clone();
        move || -> Option<(String, BackendKind)> {
            if let Some(agent_ref) = state.active_agent.get() {
                let agent =
                    state.agents.get().into_iter().find(|a| {
                        a.host_id == agent_ref.host_id && a.agent_id == agent_ref.agent_id
                    })?;
                Some((agent_ref.host_id, agent.backend_kind))
            } else {
                let host_id = state.chat_context_host_id()?;
                let settings = state.host_settings(&host_id)?;
                let backend_kind = state.draft_backend_override.get().or_else(|| {
                    settings
                        .default_backend
                        .or_else(|| settings.enabled_backends.first().copied())
                })?;
                Some((host_id, backend_kind))
            }
        }
    };

    // Returns `None` when there is no backend context or the host's schema
    // snapshot hasn't arrived yet — either way we render nothing. Only after
    // the snapshot arrives and still lacks the backend do we surface the
    // missing-schema banner, preventing a flash during host connect.
    let schema_status = {
        let state = state.clone();
        let current_binding_for_status = current_binding;
        move || -> Option<(BackendKind, Option<SessionSchemaEntry>)> {
            let (host_id, backend_kind) = current_binding_for_status()?;
            let loaded = state
                .schemas_loaded_for_host
                .get()
                .get(&host_id)
                .copied()
                .unwrap_or(false);
            let schema = state
                .session_schemas
                .get()
                .get(&host_id)
                .and_then(|schemas| schemas.get(&backend_kind))
                .cloned();
            match (loaded, schema) {
                (_, Some(schema)) => Some((backend_kind, Some(schema))),
                (true, None) => Some((backend_kind, None)),
                (false, None) => None,
            }
        }
    };

    fn header_label(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Claude => "Session Settings (Claude)",
            BackendKind::Codex => "Session Settings (Codex)",
            BackendKind::Gemini => "Session Settings (Gemini)",
            BackendKind::Kiro => "Session Settings (Kiro)",
            BackendKind::Tycode => "Session Settings",
        }
    }

    fn missing_schema_message(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Kiro => "Kiro models unavailable — check installation",
            BackendKind::Claude => "Claude settings unavailable — check installation",
            BackendKind::Codex => "Codex settings unavailable — check installation",
            BackendKind::Gemini => "Gemini settings unavailable — check installation",
            BackendKind::Tycode => "Tycode settings unavailable",
        }
    }

    fn pending_schema_message(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Kiro => "Kiro models are loading...",
            BackendKind::Claude => "Claude settings are loading...",
            BackendKind::Codex => "Codex settings are loading...",
            BackendKind::Gemini => "Gemini settings are loading...",
            BackendKind::Tycode => "Tycode settings are loading...",
        }
    }

    let current_values = {
        let state = state.clone();
        Signal::derive(move || {
            if let Some(agent_ref) = state.active_agent.get() {
                state
                    .agent_session_settings
                    .get()
                    .get(&agent_ref.agent_id)
                    .cloned()
                    .unwrap_or_default()
            } else {
                state.draft_session_settings.get()
            }
        })
    };

    let on_change_state = state.clone();
    let on_change = Callback::new(move |new_values: SessionSettingsValues| {
        if on_change_state.active_agent.get_untracked().is_some() {
            crate::actions::send_set_session_settings(&on_change_state, new_values);
        } else {
            on_change_state.draft_session_settings.set(new_values);
        }
    });

    view! {
        {move || {
            let (backend_kind, schema_entry) = schema_status()?;
            let label = header_label(backend_kind);
            Some(match schema_entry {
                Some(SessionSchemaEntry::Ready { schema }) if schema.fields.is_empty() => {
                    return None;
                }
                Some(SessionSchemaEntry::Ready { schema }) => view! {
                    <div class="session-settings-accordion">
                        <button
                            class="session-settings-toggle"
                            on:click=move |_| expanded.update(|e| *e = !*e)
                        >
                            <span class="session-settings-chevron">
                                {move || if expanded.get() { "▼" } else { "▶" }}
                            </span>
                            {label}
                        </button>
                        <Show when=move || expanded.get()>
                            <SessionSettingsControls
                                schema=schema.clone()
                                values=current_values
                                on_change=on_change
                            />
                        </Show>
                    </div>
                }.into_any(),
                Some(SessionSchemaEntry::Pending { .. }) => view! {
                    <div class="session-settings-accordion session-settings-unavailable">
                        <span class="session-settings-unavailable-text">
                            {pending_schema_message(backend_kind)}
                        </span>
                    </div>
                }.into_any(),
                Some(SessionSchemaEntry::Unavailable { message, .. }) => view! {
                    <div class="session-settings-accordion session-settings-unavailable">
                        <span class="session-settings-unavailable-text">
                            {if message.trim().is_empty() {
                                missing_schema_message(backend_kind).to_string()
                            } else {
                                message
                            }}
                        </span>
                    </div>
                }.into_any(),
                None => view! {
                    <div class="session-settings-accordion session-settings-unavailable">
                        <span class="session-settings-unavailable-text">
                            {missing_schema_message(backend_kind)}
                        </span>
                    </div>
                }.into_any(),
            })
        }}
    }
}
