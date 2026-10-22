use leptos::prelude::*;
use wasm_bindgen::JsCast;

use protocol::{
    BackendKind, ControlOption, SessionSchemaEntry, SessionSettingFieldType, SessionSettingValue,
    SessionSettingsSchema, SessionSettingsValues, TaskTokenUsageAmount, TaskTokenUsageField,
    TaskTokenUsagePayload, TaskTokenUsageScope, TaskTokenUsageStatus,
    TaskTokenUsageUnavailableReason, UNAVAILABLE_OPTION_SUFFIX,
    clear_invalid_dependent_select_values, options_including_current,
};

use crate::components::chat_message::format_compact;
use crate::components::task_usage::TaskUsagePopup;
use crate::state::{ActiveAgentRef, AppState, ComposerHandle};

#[component]
pub fn SessionSettingsControls(
    schema: SessionSettingsSchema,
    values: Signal<SessionSettingsValues>,
    on_change: Callback<SessionSettingsValues>,
    /// Stored presets (launch profiles, reviewers) can hold keys the backend's
    /// schema has since dropped. The server rejects the whole preset for them,
    /// and no field row would ever show them, so every save would resend the
    /// key with no way to repair it. Opted in by editors whose saves replace
    /// the stored values wholesale, where dropping a key really removes it.
    #[prop(optional)]
    remove_unsupported_keys: bool,
) -> impl IntoView {
    let fields = schema.fields.clone();
    let all_fields = schema.fields;
    let known_keys = all_fields
        .iter()
        .map(|field| field.key.clone())
        .collect::<Vec<_>>();
    let unsupported = Memo::new(move |_| {
        if !remove_unsupported_keys {
            return Vec::new();
        }
        let mut stale = values
            .get()
            .0
            .into_iter()
            .filter(|(key, _)| !known_keys.contains(key))
            .collect::<Vec<_>>();
        stale.sort_by(|a, b| a.0.cmp(&b.0));
        stale
    });

    view! {
        <div class="session-settings">
            {fields.into_iter().map(|field| {
                let key = field.key.clone();
                let label = field.label.clone();
                let description = field.description.clone();
                let field_type = field.field_type.clone();
                let use_slider = field.use_slider;
                let field_for_options = field.clone();
                let field_for_unknown = field.clone();
                let available_options = Memo::new(move |_| {
                    let current = values.get();
                    field_for_options
                        .select_options(&current)
                        .unwrap_or_default()
                        .to_vec()
                });
                // `select_options` answers `None` when this field's options
                // depend on another setting whose current value it does not
                // recognize — e.g. a Hermes `model` list keyed by a `profile`
                // that left the schema. Flattening that to an empty list (as
                // the memo above must, to keep one type) would render an empty
                // control with no explanation, which reads as "this backend has
                // no models" rather than "Tyde cannot tell yet".
                let options_unknown = Memo::new(move |_| {
                    let current = values.get();
                    field_for_unknown.select_options(&current).is_none()
                });
                let on_change_cb = on_change;
                let all_fields = all_fields.clone();

                view! {
                    <div
                        class="session-setting-row"
                        title=move || description.clone().unwrap_or_default()
                    >
                        <span class="session-setting-label">{label}</span>
                        {match field_type {
                            SessionSettingFieldType::Select { options: _, default, nullable } if use_slider => {
                                let current_raw = {
                                    let key = key.clone();
                                    let default = default.clone();
                                    Signal::derive(move || {
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
                                    })
                                };

                                // Built so the effective value is always one of
                                // the entries. Previously an unknown value fell
                                // through to index 0, which silently presented
                                // the first option as the current setting.
                                let entries = Memo::new(move |_| {
                                    let mut v = Vec::new();
                                    if nullable {
                                        v.push(ControlOption {
                                            value: String::new(),
                                            label: "Auto".to_string(),
                                            unavailable: false,
                                        });
                                    }
                                    let current = current_raw.get();
                                    v.extend(options_including_current(
                                        &available_options.get(),
                                        &current,
                                    ));
                                    if !v.iter().any(|entry| entry.value == current) {
                                        // Only reachable for an empty value on a
                                        // non-nullable field with no default:
                                        // genuinely unknown, and said so.
                                        v.push(ControlOption {
                                            value: current,
                                            label: "Unknown".to_string(),
                                            unavailable: true,
                                        });
                                    }
                                    v
                                });

                                let current_idx = Signal::derive(move || {
                                    let current_val = current_raw.get();
                                    entries
                                        .get()
                                        .iter()
                                        .position(|entry| entry.value == current_val)
                                        .unwrap_or(0) as i64
                                });

                                let current_label = move || {
                                    let idx = current_idx.get() as usize;
                                    entries
                                        .get()
                                        .get(idx)
                                        .map(|entry| entry.label.clone())
                                        .unwrap_or_default()
                                };

                                let on_slider_change = {
                                    let key = key.clone();
                                    let all_fields = all_fields.clone();
                                    move |ev: leptos::ev::Event| {
                                        let text = event_target_value(&ev);
                                        if let Ok(idx) = text.parse::<usize>() {
                                            let mut current = values.get_untracked();
                                            if let Some(entry) = entries
                                                .get_untracked()
                                                .get(idx)
                                                .cloned()
                                            {
                                                // The unavailable stop exists so
                                                // the slider can *show* the
                                                // session's real value. Landing
                                                // on it must not commit a value
                                                // the backend already refused.
                                                if entry.unavailable {
                                                    return;
                                                }
                                                if entry.value.is_empty() {
                                                    current.0.insert(key.clone(), SessionSettingValue::Null);
                                                } else {
                                                    current.0.insert(key.clone(), SessionSettingValue::String(entry.value));
                                                }
                                                clear_invalid_dependent_select_values(
                                                    &all_fields,
                                                    &mut current,
                                                );
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
                                            max=move || entries.get().len().saturating_sub(1).to_string()
                                            step="1"
                                            prop:value=move || current_idx.get().to_string()
                                            on:input=on_slider_change
                                        />
                                        <span class="session-setting-value-label">{current_label}</span>
                                    </div>
                                }.into_any()
                            }
                            SessionSettingFieldType::Select { options: _, default, nullable } => {
                                let key = key.clone();
                                let default_for_view = default.clone();

                                let current_value = {
                                    let key = key.clone();
                                    let default = default_for_view.clone();
                                    Signal::derive(move || {
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
                                    })
                                };

                                let on_select_change = {
                                    let key = key.clone();
                                    let all_fields = all_fields.clone();
                                    move |ev: leptos::ev::Event| {
                                        let selected = event_target_value(&ev);
                                        // The synthetic entry is rendered
                                        // disabled, so a browser will not fire
                                        // this for it — but the guard is what
                                        // makes "display-only" a property of the
                                        // commit path rather than of the markup.
                                        let unavailable = options_including_current(
                                            &available_options.get_untracked(),
                                            &selected,
                                        )
                                        .into_iter()
                                        .any(|entry| entry.value == selected && entry.unavailable);
                                        if unavailable {
                                            return;
                                        }
                                        let mut current = values.get_untracked();
                                        if selected.is_empty() {
                                            current.0.insert(key.clone(), SessionSettingValue::Null);
                                        } else {
                                            current.0.insert(key.clone(), SessionSettingValue::String(selected));
                                        }
                                        clear_invalid_dependent_select_values(
                                            &all_fields,
                                            &mut current,
                                        );
                                        on_change_cb.run(current);
                                    }
                                };

                                // A `<select>` ignores a value that has no
                                // matching `<option>` yet, and the options here
                                // are reactive children rendered after the
                                // element's properties are applied. Re-applying
                                // the value from a node ref — tracking the
                                // option list so this re-runs whenever the set
                                // changes — is what makes a late-arriving
                                // option actually take effect. Without it the
                                // control silently falls back to whichever
                                // option the browser picks.
                                let select_ref: NodeRef<leptos::html::Select> = NodeRef::new();
                                Effect::new(move |_| {
                                    let value = current_value.get();
                                    let _ = available_options.get();
                                    if let Some(element) = select_ref.get() {
                                        element.set_value(&value);
                                    }
                                });

                                view! {
                                    <select
                                        class="session-setting-select"
                                        node_ref=select_ref
                                        prop:value=move || current_value.get()
                                        on:change=on_select_change
                                    >
                                        {nullable.then(|| view! {
                                            <option value="">"Auto"</option>
                                        })}
                                        // Options include the session's own
                                        // value even when the schema stopped
                                        // offering it, so the control cannot
                                        // fall back to displaying a different
                                        // option as if it were the setting.
                                        {move || options_including_current(
                                            &available_options.get(),
                                            &current_value.get(),
                                        )
                                            .into_iter()
                                            .map(|entry| {
                                                // Selected-but-disabled: the
                                                // value is shown because it is
                                                // in effect, and disabled
                                                // because the backend cannot
                                                // serve it.
                                                let ControlOption {
                                                    value,
                                                    label,
                                                    unavailable,
                                                } = entry;
                                                view! {
                                                    <option
                                                        value={value}
                                                        disabled=unavailable
                                                    >{label}</option>
                                                }
                                            })
                                            .collect_view()}
                                        // Say why the list is empty rather than
                                        // presenting "no choices" as a fact.
                                        {move || (options_unknown.get()
                                            && available_options.get().is_empty())
                                            .then(|| view! {
                                                <option value="" disabled=true>
                                                    "Unavailable \u{2014} depends on another \
                                                     setting Tyde cannot resolve"
                                                </option>
                                            })}
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
            {move || unsupported.get().into_iter().map(|(key, value)| {
                let shown = match value {
                    SessionSettingValue::String(value) => value,
                    SessionSettingValue::Bool(value) => value.to_string(),
                    SessionSettingValue::Integer(value) => value.to_string(),
                    SessionSettingValue::Null => "unset".to_owned(),
                };
                let remove_label = format!("Remove unavailable setting {key}");
                view! {
                    <div
                        class="session-setting-row"
                        title="This backend no longer offers this setting"
                    >
                        <span class="session-setting-label">{key.clone()}</span>
                        <span class="session-setting-value-label">
                            {format!("{shown}{UNAVAILABLE_OPTION_SUFFIX}")}
                        </span>
                        <button
                            class="settings-btn"
                            aria-label=remove_label
                            on:click=move |_| {
                                let mut current = values.get_untracked();
                                current.0.remove(&key);
                                on_change.run(current);
                            }
                        >
                            "Remove"
                        </button>
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

/// Bind settings to this pane's agent/composer, not the globally focused agent.
pub(crate) fn session_settings_views(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    composer: ComposerHandle,
) -> (AnyView, AnyView) {
    let state = expect_context::<AppState>();
    let chat_appearance = state.chat_appearance;
    let expanded = RwSignal::new(false);
    let inline_trigger = NodeRef::<leptos::html::Button>::new();
    let inline_panel = NodeRef::<leptos::html::Div>::new();
    let close_inline = Callback::new(move |()| {
        expanded.set(false);
        if let Some(trigger) = inline_trigger.get_untracked()
            && let Err(error) = trigger.focus()
        {
            log::error!("Cannot return focus to session settings: {error:?}");
        }
    });
    Effect::new(move |focused: Option<bool>| {
        if !expanded.get() || chat_appearance.get() != crate::appearance::ChatAppearance::Ink {
            return false;
        }
        if focused == Some(true) {
            return true;
        }
        let Some(panel) = inline_panel.get() else {
            return false;
        };
        if let Err(error) = panel.focus() {
            log::error!("Cannot focus session settings: {error:?}");
        }
        true
    });

    // Memos, not plain closures: the outer view closure below must depend
    // only on these deduped semantic values. `current_binding` reads the raw
    // `state.agents` vec, which is notified by every activity summary, spawn,
    // and close — if the view tracked it directly, that churn would recreate
    // the footer subtree (and reset the task popover's open state) even when
    // the active agent's binding is unchanged.
    let current_binding = {
        let state = state.clone();
        let composer = composer.clone();
        Memo::new(move |_| -> Option<(String, BackendKind)> {
            if let Some(agent_ref) = agent_ref.get() {
                // `with` reads the agents Vec in place — the previous
                // `state.agents.get().into_iter().find` cloned the
                // whole Vec just to find one agent's backend_kind.
                let backend_kind = state.agents.with(|agents| {
                    agents
                        .iter()
                        .find(|a| {
                            a.host_id == agent_ref.host_id && a.agent_id == agent_ref.agent_id
                        })
                        .map(|a| a.backend_kind)
                })?;
                Some((agent_ref.host_id, backend_kind))
            } else {
                let host_id = state.chat_context_host_id()?;
                let settings = state.host_settings(&host_id)?;
                let backend_kind = composer.backend_override.get().or_else(|| {
                    settings
                        .default_backend
                        .or_else(|| settings.enabled_backends.first().copied())
                })?;
                Some((host_id, backend_kind))
            }
        })
    };

    // Returns `None` when there is no backend context or the host's schema
    // snapshot hasn't arrived yet — either way we render nothing. Only after
    // the snapshot arrives and still lacks the backend do we surface the
    // missing-schema banner, preventing a flash during host connect.
    //
    // A running agent validates edits against the schema it started with,
    // which the host catalog may have drifted from, so it renders only the
    // schema its own stream emitted.
    let schema_status = {
        let state = state.clone();
        Memo::new(
            move |_| -> Option<(BackendKind, Option<SessionSchemaEntry>)> {
                let (host_id, backend_kind) = current_binding.get()?;
                if let Some(agent_ref) = agent_ref.get() {
                    let schema = state
                        .agent_session_schemas
                        .with(|map| map.get(&agent_ref.agent_id).cloned())?;
                    return Some((
                        backend_kind,
                        schema.map(|schema| SessionSchemaEntry::Ready { schema }),
                    ));
                }
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
            },
        )
    };

    fn header_label(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Claude => "Session Settings (Claude)",
            BackendKind::Codex => "Session Settings (Codex)",
            BackendKind::Antigravity => "Session Settings (Antigravity)",
            BackendKind::Hermes => "Session Settings (Hermes)",
            BackendKind::Grok => "Session Settings (Grok)",
            BackendKind::Opencode => "Session Settings (OpenCode)",
            BackendKind::Muse => "Session Settings (Muse)",
            BackendKind::Kiro => "Session Settings (Kiro)",
            BackendKind::Tycode => "Session Settings",
        }
    }

    fn missing_schema_message(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Kiro => "Kiro models unavailable — check the agent command",
            BackendKind::Claude => "Claude settings unavailable — check installation",
            BackendKind::Codex => "Codex settings unavailable — check installation",
            BackendKind::Antigravity => "Antigravity settings unavailable — check installation",
            BackendKind::Hermes => "Hermes settings unavailable — check installation",
            BackendKind::Grok => "Grok settings unavailable — check installation",
            BackendKind::Opencode => "OpenCode settings unavailable — check installation",
            BackendKind::Muse => "Muse settings unavailable — check installation",
            BackendKind::Tycode => "Tycode settings unavailable",
        }
    }

    fn pending_schema_message(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Kiro => "Kiro models are loading...",
            BackendKind::Claude => "Claude settings are loading...",
            BackendKind::Codex => "Codex settings are loading...",
            BackendKind::Antigravity => "Antigravity settings are loading...",
            BackendKind::Hermes => "Hermes settings are loading...",
            BackendKind::Grok => "Grok settings are loading...",
            BackendKind::Opencode => "OpenCode settings are loading...",
            BackendKind::Muse => "Muse settings are loading...",
            BackendKind::Tycode => "Tycode settings are loading...",
        }
    }

    let current_values = {
        let state = state.clone();
        let composer = composer.clone();
        Signal::derive(move || {
            if let Some(agent_ref) = agent_ref.get() {
                state
                    .agent_session_settings
                    .get()
                    .get(&agent_ref.agent_id)
                    .cloned()
                    .unwrap_or_default()
            } else {
                composer.session_settings.get()
            }
        })
    };

    // Server rollup for the active agent's whole task (root + sub-agents). This
    // is the single authoritative token-usage source for the footer; the
    // frontend never sums chat rows. The rollup is keyed by root agent, so when
    // the active agent is a root we hit it directly; when it's a selected
    // sub-agent we fall back to the same-host rollup whose breakdown contains
    // it, so the footer always reflects the task-wide total. `has_task_rollup`
    // is the deduped presence bit the outer view keys off — payload-content
    // changes must not recreate the header (that would reset the display's
    // open/hover state every server tick).
    let task_rollup = {
        let state = state.clone();
        Memo::new(move |_| -> Option<TaskTokenUsagePayload> {
            let agent_ref = agent_ref.get()?;
            state.task_token_usage.with(|map| {
                if let Some(payload) = map.get(&agent_ref) {
                    return Some(payload.clone());
                }
                map.iter()
                    .find(|(key, payload)| {
                        key.host_id == agent_ref.host_id
                            && payload
                                .breakdown
                                .iter()
                                .any(|entry| entry.agent_id == agent_ref.agent_id)
                    })
                    .map(|(_, payload)| payload.clone())
            })
        })
    };
    let has_task_rollup = Memo::new(move |_| task_rollup.with(|payload| payload.is_some()));

    let on_change_state = state.clone();
    let on_change_composer = composer.clone();
    let on_change = Callback::new(move |new_values: SessionSettingsValues| {
        if let Some(target) = agent_ref.get_untracked() {
            crate::actions::send_set_session_settings(&on_change_state, target, new_values);
        } else {
            // A user edit to the draft: mark dirty so spawn sends these as
            // explicit overrides even when a launch profile is selected.
            on_change_composer.session_settings_dirty.set(true);
            on_change_composer.session_settings.set(new_values);
        }
    });

    // A schema-status text row (pending/unavailable/missing) with the task
    // token usage on the right. These rows keep rendering even without a
    // rollup — the usage display just renders nothing then.
    fn status_row(
        text: String,
        rollup: Memo<Option<TaskTokenUsagePayload>>,
        binding: Memo<Option<(String, BackendKind)>>,
    ) -> AnyView {
        view! {
            <div class="session-settings-accordion session-settings-unavailable">
                <span class="session-settings-unavailable-text">{text}</span>
                <TaskUsageBadge rollup=rollup binding=binding />
            </div>
        }
        .into_any()
    }

    // A header row carrying only the task token usage, for agents whose backend
    // has no session-settings surface (empty schema, or none yet). Rendered
    // only when a rollup exists so there's never a useless empty footer row.
    fn badge_only_row(
        has_rollup: bool,
        rollup: Memo<Option<TaskTokenUsagePayload>>,
        binding: Memo<Option<(String, BackendKind)>>,
    ) -> Option<AnyView> {
        has_rollup.then(|| {
            view! {
                <div class="session-settings-accordion">
                    <div class="session-settings-header">
                        <TaskUsageBadge rollup=rollup binding=binding />
                    </div>
                </div>
            }
            .into_any()
        })
    }

    let inline = view! {
        <div class="ink-session-slot">
            <Show when=move || chat_appearance.get() == crate::appearance::ChatAppearance::Ink>
            {move || match schema_status.get() {
                Some((backend, Some(SessionSchemaEntry::Ready { schema }))) if !schema.fields.is_empty() => {
                    let label = header_label(backend);
                    view! {
                        <button
                            type="button"
                            class="ink-session-toggle"
                            node_ref=inline_trigger
                            aria-label="Model and session settings"
                            title=label
                            aria-haspopup="dialog"
                            aria-expanded=move || expanded.get().to_string()
                            on:click=move |_| expanded.update(|value| *value = !*value)
                        >
                            <span>{crate::components::agents_panel::backend_label(backend)}</span>
                            <span aria-hidden="true">"⌄"</span>
                        </button>
                    }.into_any()
                }
                _ => ().into_any(),
            }}
            </Show>
        </div>
    }.into_any();

    let footer = view! {
        {move || {
            let has_rollup = has_task_rollup.get();
            match schema_status.get() {
                Some((backend_kind, Some(SessionSchemaEntry::Ready { schema })))
                    if !schema.fields.is_empty() =>
                {
                    let label = header_label(backend_kind);
                    Some(view! {
                        <div class="session-settings-accordion">
                            <div class="session-settings-header">
                                <button
                                    class="session-settings-toggle"
                                    on:click=move |_| expanded.update(|e| *e = !*e)
                                >
                                    <span class="session-settings-chevron">
                                        {move || if expanded.get() { "▼" } else { "▶" }}
                                    </span>
                                    {label}
                                </button>
                                <TaskUsageBadge rollup=task_rollup binding=current_binding />
                            </div>
                            <Show when=move || expanded.get()>
                                <Show when=move || chat_appearance.get() == crate::appearance::ChatAppearance::Ink>
                                    <div class="ink-session-backdrop" on:click=move |_| close_inline.run(())></div>
                                </Show>
                                <div class="session-settings-panel"
                                    node_ref=inline_panel
                                    role=move || (chat_appearance.get() == crate::appearance::ChatAppearance::Ink).then_some("dialog")
                                    aria-label=move || (chat_appearance.get() == crate::appearance::ChatAppearance::Ink).then_some("Model and session settings")
                                    tabindex=move || (chat_appearance.get() == crate::appearance::ChatAppearance::Ink).then_some("-1")
                                    on:keydown=move |event: leptos::ev::KeyboardEvent| {
                                        if event.key() == "Escape" && chat_appearance.get() == crate::appearance::ChatAppearance::Ink {
                                            event.prevent_default();
                                            close_inline.run(());
                                        }
                                    }
                                >
                                    <Show when=move || chat_appearance.get() == crate::appearance::ChatAppearance::Ink>
                                    <div class="ink-session-panel-heading">
                                        <span>"Model and session settings"</span>
                                        <button type="button" aria-label="Close session settings" on:click=move |_| close_inline.run(())>"×"</button>
                                    </div>
                                    </Show>
                                    <SessionSettingsControls
                                        schema=schema.clone()
                                        values=current_values
                                        on_change=on_change
                                    />
                                </div>
                            </Show>
                        </div>
                    }.into_any())
                }
                Some((_, Some(SessionSchemaEntry::Ready { .. }))) => {
                    badge_only_row(has_rollup, task_rollup, current_binding)
                }
                Some((backend_kind, Some(SessionSchemaEntry::Pending { .. }))) => Some(status_row(
                    pending_schema_message(backend_kind).to_string(),
                    task_rollup,
                    current_binding,
                )),
                Some((backend_kind, Some(SessionSchemaEntry::Unavailable { message, .. }))) => {
                    let text = if message.trim().is_empty() {
                        missing_schema_message(backend_kind).to_string()
                    } else {
                        message
                    };
                    Some(status_row(text, task_rollup, current_binding))
                }
                Some((backend_kind, None)) => Some(status_row(
                    missing_schema_message(backend_kind).to_string(),
                    task_rollup,
                    current_binding,
                )),
                None => badge_only_row(has_rollup, task_rollup, current_binding),
            }
        }}
    }.into_any();
    (inline, footer)
}

pub(super) fn task_usage_unavailable_text(reason: TaskTokenUsageUnavailableReason) -> &'static str {
    match reason {
        TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted => "no completed turns",
        TaskTokenUsageUnavailableReason::BackendDidNotReport => "backend did not report",
        TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous => "provider scope ambiguous",
        TaskTokenUsageUnavailableReason::AgentStarting => "agent starting",
        TaskTokenUsageUnavailableReason::AgentUnavailable => "agent unavailable",
        TaskTokenUsageUnavailableReason::HelperAccountingUnavailable => {
            "analyzer accounting unavailable"
        }
        TaskTokenUsageUnavailableReason::SharedSessionAccounting => {
            "shared session accounting cannot be attributed"
        }
        TaskTokenUsageUnavailableReason::ObservationInterrupted => "agent observations interrupted",
        TaskTokenUsageUnavailableReason::AccountingHistoryIncomplete => {
            "saved accounting incomplete"
        }
    }
}

// Keep the footer's established arrow/subset presentation. Prompt totals
// arrive already aggregated: missing cache breakdowns must not erase input
// contributed by other backends or turn an unknown subset into zero.
fn task_number(
    amount: &TaskTokenUsageAmount,
    field: TaskTokenUsageField,
    value: Option<u64>,
) -> String {
    match value {
        Some(value) if amount.partial_fields.contains(&field) => {
            format!("≥{}", format_compact(value))
        }
        Some(value) => format_compact(value),
        None => "—".to_owned(),
    }
}

fn task_amount_texts(amount: &TaskTokenUsageAmount) -> (String, Option<String>) {
    let input = match amount.cached_prompt_tokens {
        Some(cached)
            if cached > 0
                || amount
                    .partial_fields
                    .contains(&TaskTokenUsageField::CachedPrompt) =>
        {
            format!(
                "↑{} (cached {})",
                task_number(amount, TaskTokenUsageField::Prompt, amount.prompt_tokens),
                task_number(amount, TaskTokenUsageField::CachedPrompt, Some(cached))
            )
        }
        _ => format!(
            "↑{}",
            task_number(amount, TaskTokenUsageField::Prompt, amount.prompt_tokens)
        ),
    };
    let output = match amount.reasoning_tokens {
        Some(reasoning)
            if reasoning > 0
                || amount
                    .partial_fields
                    .contains(&TaskTokenUsageField::Reasoning) =>
        {
            format!(
                "↓{} (reasoning {})",
                task_number(amount, TaskTokenUsageField::Output, amount.output_tokens),
                task_number(amount, TaskTokenUsageField::Reasoning, Some(reasoning))
            )
        }
        _ => format!(
            "↓{}",
            task_number(amount, TaskTokenUsageField::Output, amount.output_tokens)
        ),
    };
    (input, Some(output))
}

pub(super) fn task_detail_tokens(amount: &TaskTokenUsageAmount) -> AnyView {
    let prompt = task_number(amount, TaskTokenUsageField::Prompt, amount.prompt_tokens);
    let cached = task_number(
        amount,
        TaskTokenUsageField::CachedPrompt,
        amount.cached_prompt_tokens,
    );
    let output = task_number(amount, TaskTokenUsageField::Output, amount.output_tokens);
    let reasoning = task_number(
        amount,
        TaskTokenUsageField::Reasoning,
        amount.reasoning_tokens,
    );
    view! {
        <span class="token-stat token-stat-input">{format!("↑{prompt}")}<span class="task-token-detail">{format!(" (cached {cached})")}</span></span>
        <span class="token-stat token-stat-output">{format!("↓{output}")}<span class="task-token-detail">{format!(" (reasoning {reasoning})")}</span></span>
    }.into_any()
}

pub(super) fn task_scope_view(scope: &TaskTokenUsageScope) -> AnyView {
    match scope {
        TaskTokenUsageScope::Known { usage } => {
            let tokens = task_detail_tokens(usage);
            view! {
                <span class="session-task-row-usage">
                    {tokens}
                    {(usage.prompt_tokens.is_none() && usage.output_tokens.is_none()).then(|| view! { <span>{format!("Reported combined: {} · input/output unreported", format_compact(usage.total_tokens))}</span> })}
                    {(!usage.partial_fields.is_empty()).then(|| view! { <crate::components::task_usage::UsageInfo label="partial breakdown".to_owned() detail="≥ marks the reported subtotal; some contributing usage has no breakdown for that field. — means unreported, not zero.".to_owned() /> })}
                </span>
            }
            .into_any()
        }
        TaskTokenUsageScope::Partial {
            usage,
            unavailable_count,
            ..
        } => {
            let tokens = task_detail_tokens(usage);
            let marker = format!("partial \u{b7} {unavailable_count} unreported");
            view! {
                <span class="session-task-row-usage">
                    {tokens}
                    {(usage.prompt_tokens.is_none() && usage.output_tokens.is_none()).then(|| view! { <span>{format!("Reported combined: {} · input/output unreported", format_compact(usage.total_tokens))}</span> })}
                    {(!usage.partial_fields.is_empty()).then(|| view! { <crate::components::task_usage::UsageInfo label="partial breakdown".to_owned() detail="≥ marks the reported subtotal; some contributing usage has no breakdown for that field. — means unreported, not zero.".to_owned() /> })}
                    <span class="session-task-status session-task-status-partial">
                        {marker}
                    </span>
                </span>
            }
            .into_any()
        }
        TaskTokenUsageScope::Unavailable { reason } => view! {
            <span class="session-task-row-usage session-task-row-unavailable">
                {task_usage_unavailable_text(*reason)}
            </span>
        }
        .into_any(),
    }
}

/// The footer remains the authoritative task total; detail views never replace it.
#[component]
fn TaskUsageBadge(
    rollup: Memo<Option<TaskTokenUsagePayload>>,
    binding: Memo<Option<(String, BackendKind)>>,
) -> impl IntoView {
    let chat_appearance = expect_context::<AppState>().chat_appearance;
    let open = RwSignal::new(false);
    let wrapper_ref: NodeRef<leptos::html::Span> = NodeRef::new();
    let toggle_ref: NodeRef<leptos::html::Button> = NodeRef::new();
    let close = Callback::new(move |_| {
        open.set(false);
        if let Some(toggle) = toggle_ref.get_untracked() {
            let _ = toggle.focus();
        }
    });
    let outside_pointer = window_event_listener(leptos::ev::pointerdown, move |event| {
        if !open.try_get_untracked().unwrap_or(false) {
            return;
        }
        let inside = wrapper_ref
            .try_get_untracked()
            .flatten()
            .is_some_and(|wrapper| {
                event
                    .target()
                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    .is_some_and(|node| wrapper.contains(Some(&node)))
            });
        if !inside {
            open.try_set(false);
        }
    });
    let escape_key = window_event_listener(leptos::ev::keydown, move |event| {
        if event.key() == "Escape" && open.try_get_untracked().unwrap_or(false) {
            close.run(());
        }
    });
    on_cleanup(move || {
        outside_pointer.remove();
        escape_key.remove();
    });
    let popover_id = Memo::new(move |_| {
        rollup
            .get()
            .map(|payload| format!("session-task-popover-{}", payload.root_agent_id))
    });
    view! {
        <Show when=move || chat_appearance.get() != crate::appearance::ChatAppearance::Ink || rollup.with(|payload| payload.is_some())>
        <span class="session-task-usage" node_ref=wrapper_ref>
            <button class="session-task-toggle" node_ref=toggle_ref aria-haspopup="dialog"
                aria-expanded=move || open.get().to_string() aria-controls=move || popover_id.get()
                title="Cumulative tokens across this task's requests and agents. The context panel shows the latest request's occupancy instead. ≥ marks a reported subtotal with incomplete breakdown coverage."
                on:click=move |_| open.update(|open| *open = !*open)>
                <span class="session-task-scope">"Task"</span>
                {move || rollup.get().map(|payload| match payload.total.status {
                    TaskTokenUsageStatus::Unavailable { .. } => view! { <span class="session-task-usage-unavailable">"usage unavailable"</span> }.into_any(),
                    _ => {
                        let (input_text, output_text) = task_amount_texts(&payload.total.usage);
                        view! { <span class="token-stat token-stat-input">{input_text}</span>{output_text.map(|text| view! { <span class="token-stat token-stat-output">{text}</span> })} }.into_any()
                    }
                })}
                {move || rollup.get().and_then(|payload| (payload.breakdown.len() > 1).then(|| view! { <span class="session-task-including">{format!("including {} agents", payload.breakdown.len())}</span> }))}
                {move || rollup.get().and_then(|payload| (matches!(payload.total.status, TaskTokenUsageStatus::Partial { .. }) || !payload.total.usage.partial_fields.is_empty()).then(|| view! { <span class="session-task-status session-task-status-partial">"partial"</span> }))}
            </button>
            <Show when=move || open.get()>
                <div id=move || popover_id.get()><TaskUsagePopup rollup=rollup binding=binding on_close=close /></div>
            </Show>
        </span>
        </Show>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AgentInfo, TabContent};
    use leptos::mount::mount_to;
    use protocol::{
        AgentActivitySummaryPayload, AgentActivitySummaryState, AgentId, AgentOrigin, Envelope,
        FrameKind, LaunchProfile, LaunchProfileId, LaunchProfileKind, SelectOption,
        SelectOptionsBySetting, SelectOptionsForValue, SessionSettingField,
        SetSessionSettingsPayload, StreamPath, TaskTokenUsageAggregate, TaskTokenUsageEntry,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn model_change_clears_unsupported_dependent_select_value() {
        let fields = vec![
            SessionSettingField {
                key: "model".to_owned(),
                label: "Model".to_owned(),
                description: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![
                        SelectOption {
                            value: "model-a".to_owned(),
                            label: "Model A".to_owned(),
                        },
                        SelectOption {
                            value: "model-b".to_owned(),
                            label: "Model B".to_owned(),
                        },
                    ],
                    default: None,
                    nullable: true,
                },
                use_slider: false,
                select_options_by_setting: None,
            },
            SessionSettingField {
                key: "effort".to_owned(),
                label: "Effort".to_owned(),
                description: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![SelectOption {
                        value: "max".to_owned(),
                        label: "Max".to_owned(),
                    }],
                    default: None,
                    nullable: true,
                },
                use_slider: true,
                select_options_by_setting: Some(SelectOptionsBySetting {
                    setting_key: "model".to_owned(),
                    values: vec![
                        SelectOptionsForValue {
                            setting_value: "model-a".to_owned(),
                            options: vec![SelectOption {
                                value: "max".to_owned(),
                                label: "Max".to_owned(),
                            }],
                        },
                        SelectOptionsForValue {
                            setting_value: "model-b".to_owned(),
                            options: vec![SelectOption {
                                value: "high".to_owned(),
                                label: "High".to_owned(),
                            }],
                        },
                    ],
                }),
            },
        ];
        let mut values = SessionSettingsValues::default();
        values.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("model-b".to_owned()),
        );
        values.0.insert(
            "effort".to_owned(),
            SessionSettingValue::String("max".to_owned()),
        );

        clear_invalid_dependent_select_values(&fields, &mut values);

        assert_eq!(values.0.get("effort"), Some(&SessionSettingValue::Null));
    }

    /// A session's effective value must stay visible even after the schema
    /// stops offering it — the Hermes case where a named profile's gateway
    /// probe fails and the profile leaves the schema. Dropping the value lets
    /// the control display a *different* option as though it were the setting.
    #[wasm_bindgen_test]
    fn current_value_absent_from_options_is_kept_and_marked() {
        let options = vec![SelectOption {
            value: "default".to_owned(),
            label: "Default".to_owned(),
        }];

        let entries = options_including_current(&options, "qatest");
        assert_eq!(
            entries.len(),
            2,
            "the session's own value must be added, not dropped: {entries:?}"
        );
        assert_eq!(
            entries[1].value, "qatest",
            "the injected entry keeps the raw value"
        );
        assert!(
            entries[1].label.contains("qatest") && entries[1].label.contains("unavailable"),
            "the injected entry must name the value and mark it unavailable, got: {}",
            entries[1].label
        );
        assert!(
            entries[1].unavailable,
            "the injected entry must be flagged so commit paths can refuse it"
        );
        assert_eq!(
            entries[0].value, "default",
            "schema options are preserved ahead of the injected entry"
        );
        assert!(
            !entries[0].unavailable,
            "a real schema option is actionable"
        );

        // A value the schema does offer is untouched — no phantom duplicate.
        let known = options_including_current(&options, "default");
        assert_eq!(known.len(), 1, "a recognized value must not be duplicated");
        assert_eq!(known[0].label, "Default", "and must not be relabelled");
        assert!(!known[0].unavailable);
        // Empty means "unset"; that is the nullable/Auto case, not an unknown.
        assert_eq!(
            options_including_current(&options, "").len(),
            1,
            "an empty value must not inject an entry"
        );
    }

    /// R-02: a backend can retain an option it cannot serve so its reason stays
    /// visible — the Hermes profile schema emits `"<name> — Unavailable:
    /// <error>"` for a profile whose gateway probe failed. Such an option is
    /// schema-provided, so it is not the synthetic entry, and it must still be
    /// non-actionable.
    #[wasm_bindgen_test]
    fn schema_provided_unavailable_option_is_flagged() {
        let options = vec![
            SelectOption {
                value: "default".to_owned(),
                label: "Default".to_owned(),
            },
            SelectOption {
                value: "gpt".to_owned(),
                label: "gpt \u{2014} Unavailable: gateway exploded".to_owned(),
            },
        ];

        let entries = options_including_current(&options, "default");
        assert_eq!(entries.len(), 2, "both schema options are kept");
        assert!(
            !entries[0].unavailable,
            "a healthy schema option stays actionable"
        );
        assert!(
            entries[1].unavailable,
            "an option the backend marked unavailable must not be actionable: {:?}",
            entries[1]
        );
        assert!(
            entries[1].label.contains("gateway exploded"),
            "the backend's reason must survive to the user"
        );

        // The same option selected as the current value is still unavailable,
        // and must not be duplicated by the synthetic-entry path.
        let entries = options_including_current(&options, "gpt");
        assert_eq!(entries.len(), 2, "a present value must not be re-injected");
        assert!(entries[1].unavailable);
    }

    /// End-to-end for M5: a session carrying `profile=qatest` against a schema
    /// that only knows `default` must render `qatest`, not `Default`. Asserts
    /// on what the user sees and on the control's effective value.
    #[wasm_bindgen_test]
    async fn named_profile_absent_from_schema_is_not_displayed_as_default() {
        let container = make_container();
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Hermes,
            fields: vec![SessionSettingField {
                key: "profile".to_owned(),
                label: "Profile".to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![SelectOption {
                        value: "default".to_owned(),
                        label: "Default".to_owned(),
                    }],
                    default: Some("default".to_owned()),
                    nullable: false,
                },
            }],
        };
        let mut values = SessionSettingsValues::default();
        values.0.insert(
            "profile".to_owned(),
            SessionSettingValue::String("qatest".to_owned()),
        );
        let values = RwSignal::new(values);

        let _handle = mount_to(container.clone(), move || {
            view! {
                <SessionSettingsControls
                    schema=schema.clone()
                    values=values.into()
                    on_change=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("qatest"),
            "the session's real profile must be visible, got: {text}"
        );

        let select: web_sys::HtmlSelectElement = query(&container, ".session-setting-select")
            .expect("the profile select should render")
            .dyn_into()
            .unwrap();
        assert_eq!(
            select.value(),
            "qatest",
            "the control's effective value must be the session's profile, not the schema default"
        );
        assert!(
            select.selected_index() >= 0,
            "the control must resolve to a real option rather than an unmatched blank"
        );
    }

    /// The synthetic entry is display-only. It must render disabled, and
    /// selecting it must not reach `on_change` — otherwise the UI invites the
    /// user to "set" a value the backend has already said it cannot serve.
    #[wasm_bindgen_test]
    async fn unavailable_option_is_rendered_disabled_and_is_not_committable() {
        let container = make_container();
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Hermes,
            fields: vec![SessionSettingField {
                key: "profile".to_owned(),
                label: "Profile".to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![SelectOption {
                        value: "default".to_owned(),
                        label: "Default".to_owned(),
                    }],
                    default: Some("default".to_owned()),
                    nullable: false,
                },
            }],
        };
        let mut values = SessionSettingsValues::default();
        values.0.insert(
            "profile".to_owned(),
            SessionSettingValue::String("qatest".to_owned()),
        );
        let values = RwSignal::new(values);
        let commits = RwSignal::new(0_usize);

        let _handle = mount_to(container.clone(), move || {
            view! {
                <SessionSettingsControls
                    schema=schema.clone()
                    values=values.into()
                    on_change=Callback::new(move |_| commits.update(|n| *n += 1))
                />
            }
        });
        next_tick().await;

        let options = container.query_selector_all("option").unwrap();
        let mut saw_disabled_unavailable = false;
        for index in 0..options.length() {
            let option: web_sys::HtmlOptionElement = options
                .item(index)
                .unwrap()
                .dyn_into()
                .expect("every child is an option");
            let label = option.text();
            if label.contains("qatest") {
                assert!(
                    option.disabled(),
                    "the unavailable entry must not be selectable, got enabled: {label}"
                );
                saw_disabled_unavailable = true;
            } else {
                assert!(
                    !option.disabled(),
                    "real schema options stay actionable, got disabled: {label}"
                );
            }
        }
        assert!(
            saw_disabled_unavailable,
            "the session's unavailable value must still be present in the list"
        );

        // Even if a change event reaches the handler (a browser that ignores
        // `disabled`, or a synthesized event), the commit path must refuse it.
        let select: web_sys::HtmlSelectElement = query(&container, ".session-setting-select")
            .expect("the profile select should render")
            .dyn_into()
            .unwrap();
        select.set_value("qatest");
        let event = web_sys::Event::new("change").unwrap();
        select.dispatch_event(&event).unwrap();
        next_tick().await;
        assert_eq!(
            commits.get_untracked(),
            0,
            "selecting an unavailable value must never be submitted as an edit"
        );
    }

    /// R-02 end-to-end: the retained-unavailable *schema* option (the shape M4
    /// actually produces) renders disabled and cannot be committed. The
    /// previous coverage fixtured only a synthetic value absent from the
    /// schema, which is a different shape and left this path unguarded.
    #[wasm_bindgen_test]
    async fn schema_unavailable_profile_renders_disabled_and_is_not_committable() {
        let container = make_container();
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Hermes,
            fields: vec![SessionSettingField {
                key: "profile".to_owned(),
                label: "Profile".to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![
                        SelectOption {
                            value: "default".to_owned(),
                            label: "Default".to_owned(),
                        },
                        SelectOption {
                            value: "gpt".to_owned(),
                            label: "gpt \u{2014} Unavailable: gateway exploded".to_owned(),
                        },
                    ],
                    default: Some("default".to_owned()),
                    nullable: false,
                },
            }],
        };
        let mut values = SessionSettingsValues::default();
        values.0.insert(
            "profile".to_owned(),
            SessionSettingValue::String("default".to_owned()),
        );
        let values = RwSignal::new(values);
        let commits = RwSignal::new(0_usize);

        let _handle = mount_to(container.clone(), move || {
            view! {
                <SessionSettingsControls
                    schema=schema.clone()
                    values=values.into()
                    on_change=Callback::new(move |_| commits.update(|n| *n += 1))
                />
            }
        });
        next_tick().await;

        let options = container.query_selector_all("option").unwrap();
        let mut checked = 0;
        for index in 0..options.length() {
            let option: web_sys::HtmlOptionElement =
                options.item(index).unwrap().dyn_into().unwrap();
            if option.value() == "gpt" {
                assert!(
                    option.disabled(),
                    "a backend-marked unavailable profile must not be selectable"
                );
                assert!(
                    option.text().contains("gateway exploded"),
                    "its reason must remain visible, got: {}",
                    option.text()
                );
                checked += 1;
            } else if option.value() == "default" {
                assert!(!option.disabled(), "the healthy profile stays selectable");
                checked += 1;
            }
        }
        assert_eq!(checked, 2, "both schema options must render");

        let select: web_sys::HtmlSelectElement = query(&container, ".session-setting-select")
            .expect("the profile select should render")
            .dyn_into()
            .unwrap();
        select.set_value("gpt");
        select
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(
            commits.get_untracked(),
            0,
            "a profile the backend cannot serve must never be committed"
        );

        // The healthy option still commits, so the guard is not a blanket block.
        select.set_value("default");
        select
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(
            commits.get_untracked(),
            1,
            "an available profile must still be committable"
        );
    }

    /// The slider's unavailable stop exists so the control can *show* the
    /// session's real value. Landing on it must not commit it.
    #[wasm_bindgen_test]
    async fn slider_does_not_commit_an_unavailable_value() {
        let container = make_container();
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Codex,
            fields: vec![SessionSettingField {
                key: "effort".to_owned(),
                label: "Effort".to_owned(),
                description: None,
                use_slider: true,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![SelectOption {
                        value: "low".to_owned(),
                        label: "Low".to_owned(),
                    }],
                    default: Some("low".to_owned()),
                    nullable: false,
                },
            }],
        };
        let mut values = SessionSettingsValues::default();
        values.0.insert(
            "effort".to_owned(),
            SessionSettingValue::String("max".to_owned()),
        );
        let values = RwSignal::new(values);
        let commits = RwSignal::new(0_usize);

        let _handle = mount_to(container.clone(), move || {
            view! {
                <SessionSettingsControls
                    schema=schema.clone()
                    values=values.into()
                    on_change=Callback::new(move |_| commits.update(|n| *n += 1))
                />
            }
        });
        next_tick().await;

        let slider: web_sys::HtmlInputElement = query(&container, ".session-setting-slider")
            .expect("the slider should render")
            .dyn_into()
            .unwrap();
        // Index 1 is the injected `max (unavailable)` stop.
        slider.set_value("1");
        slider
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(
            commits.get_untracked(),
            0,
            "the unavailable stop is display-only and must not be committed"
        );

        // A real stop still commits, so the guard did not disable the control.
        slider.set_value("0");
        slider
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(
            commits.get_untracked(),
            1,
            "an available stop must still be committable"
        );
    }

    /// F-04: the node-ref effect exists to reapply the value once a matching
    /// option arrives *after* mount. This mounts a dependent select whose
    /// options are unresolvable, then makes the parent value resolve, and
    /// asserts the live control follows. A regression in effect scheduling or
    /// node-ref timing leaves this red where a static-schema test stays green.
    #[wasm_bindgen_test]
    async fn select_adopts_an_option_that_arrives_after_mount() {
        let container = make_container();
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Hermes,
            fields: vec![SessionSettingField {
                key: "model".to_owned(),
                label: "Model".to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: Some(SelectOptionsBySetting {
                    setting_key: "profile".to_owned(),
                    values: vec![SelectOptionsForValue {
                        setting_value: "default".to_owned(),
                        options: vec![SelectOption {
                            value: "model-a".to_owned(),
                            label: "Model A".to_owned(),
                        }],
                    }],
                }),
                field_type: SessionSettingFieldType::Select {
                    options: vec![],
                    default: None,
                    nullable: true,
                },
            }],
        };
        // `profile=unknown` has no entry in `select_options_by_setting`, so
        // `select_options` answers None and the model list is unresolvable.
        let mut initial = SessionSettingsValues::default();
        initial.0.insert(
            "profile".to_owned(),
            SessionSettingValue::String("unknown".to_owned()),
        );
        initial.0.insert(
            "model".to_owned(),
            SessionSettingValue::String("model-a".to_owned()),
        );
        let values = RwSignal::new(initial);

        let _handle = mount_to(container.clone(), move || {
            view! {
                <SessionSettingsControls
                    schema=schema.clone()
                    values=values.into()
                    on_change=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        let select: web_sys::HtmlSelectElement = query(&container, ".session-setting-select")
            .expect("the model select should render")
            .dyn_into()
            .unwrap();
        assert_eq!(
            select.value(),
            "model-a",
            "before the options resolve, the effective value is still shown"
        );

        // The parent resolves; `model-a` now arrives as a real schema option.
        values.update(|v| {
            v.0.insert(
                "profile".to_owned(),
                SessionSettingValue::String("default".to_owned()),
            );
        });
        next_tick().await;
        next_tick().await;

        assert_eq!(
            select.value(),
            "model-a",
            "the late-arriving option must be adopted, not left unmatched"
        );
        let selected = select.selected_index();
        assert!(selected >= 0, "the control must resolve to a real option");
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Model A"),
            "the resolved option's real label must now be shown, got: {text}"
        );
        assert!(
            !text.contains("model-a (unavailable)"),
            "the synthetic entry must disappear once the real option exists, got: {text}"
        );
    }

    /// The slider path had its own coercion: an unresolvable value fell through
    /// to `.unwrap_or(0)`, silently presenting the first option as current.
    #[wasm_bindgen_test]
    async fn slider_does_not_coerce_an_unknown_value_to_the_first_option() {
        let container = make_container();
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Codex,
            fields: vec![SessionSettingField {
                key: "effort".to_owned(),
                label: "Effort".to_owned(),
                description: None,
                use_slider: true,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: vec![
                        SelectOption {
                            value: "low".to_owned(),
                            label: "Low".to_owned(),
                        },
                        SelectOption {
                            value: "high".to_owned(),
                            label: "High".to_owned(),
                        },
                    ],
                    default: Some("low".to_owned()),
                    nullable: false,
                },
            }],
        };
        let mut values = SessionSettingsValues::default();
        values.0.insert(
            "effort".to_owned(),
            SessionSettingValue::String("max".to_owned()),
        );
        let values = RwSignal::new(values);

        let _handle = mount_to(container.clone(), move || {
            view! {
                <SessionSettingsControls
                    schema=schema.clone()
                    values=values.into()
                    on_change=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;

        let label = query(&container, ".session-setting-value-label")
            .expect("the slider value label should render")
            .text_content()
            .unwrap_or_default();
        assert!(
            label.contains("max"),
            "the slider must report the effective value, got: {label}"
        );
        assert!(
            !label.contains("Low"),
            "an unknown value must not be coerced to the first option, got: {label}"
        );
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 600px; height: 400px;",
            )
            .unwrap();
        // Geometry must use production CSS, not styles leaked by another test.
        let style = document.create_element("style").unwrap();
        style.set_text_content(Some(include_str!("../../styles.css")));
        container.append_child(&style).unwrap();
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

    fn pointer_event(kind: &str, pointer_type: &str, bubbles: bool) -> web_sys::PointerEvent {
        let init = web_sys::PointerEventInit::new();
        init.set_bubbles(bubbles);
        init.set_pointer_type(pointer_type);
        web_sys::PointerEvent::new_with_event_init_dict(kind, &init).expect("pointer event")
    }

    /// App state with a connected host, a Ready Claude schema (with a field
    /// when `with_settings_fields`, empty otherwise — the Tycode-style "no
    /// session settings" shape), and one started agent whose chat tab is
    /// active — the active-agent Memo derives from that tab.
    fn make_state_with_active_agent(host_id: &str, agent_id: &str) -> AppState {
        make_state(host_id, agent_id, true)
    }

    fn make_state(host_id: &str, agent_id: &str, with_settings_fields: bool) -> AppState {
        make_state_for_backend(host_id, agent_id, with_settings_fields, BackendKind::Claude)
    }

    fn make_state_for_backend(
        host_id: &str,
        agent_id: &str,
        with_settings_fields: bool,
        backend_kind: BackendKind,
    ) -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_streams.update(|map| {
            map.insert(host_id.to_owned(), StreamPath(format!("/host/{host_id}")));
        });
        state.connection_statuses.update(|map| {
            map.insert(
                host_id.to_owned(),
                crate::state::ConnectionStatus::Connected,
            );
        });
        crate::dispatch::prime_host_for_tests(&state, host_id);
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                tyde_permissions: Default::default(),
                host_id: host_id.to_owned(),
                agent_id: AgentId(agent_id.to_owned()),
                name: "Root".to_owned(),
                origin: AgentOrigin::User,
                backend_kind,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                team_member_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                instance_stream: StreamPath(format!("/agent/{agent_id}/inst")),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
                review_recipient: false,
            });
        });
        let fields = if with_settings_fields {
            vec![SessionSettingField {
                key: "verbose".to_owned(),
                label: "Verbose".to_owned(),
                description: None,
                field_type: SessionSettingFieldType::Toggle { default: false },
                use_slider: false,
                select_options_by_setting: None,
            }]
        } else {
            Vec::new()
        };
        let schema = SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind,
            fields,
        };
        state.session_schemas.update(|map| {
            map.entry(host_id.to_owned()).or_default().insert(
                backend_kind,
                SessionSchemaEntry::Ready {
                    schema: schema.clone(),
                },
            );
        });
        state.agent_session_schemas.update(|map| {
            map.insert(AgentId(agent_id.to_owned()), Some(schema));
        });
        state.schemas_loaded_for_host.update(|map| {
            map.insert(host_id.to_owned(), true);
        });
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: AgentId(agent_id.to_owned()),
            }),
            "Root".to_owned(),
            true,
        );
        state
    }

    /// Mounts the bar the way a chat pane does: bound to that pane's own agent
    /// and composer rather than to a global active agent.
    fn mount_bar(container: &HtmlElement, state: AppState) -> impl Sized {
        let state_for_mount = state;
        mount_to(container.clone(), move || {
            let state = state_for_mount.clone();
            let agent_state = state.clone();
            let agent_ref = Signal::derive(move || agent_state.active_agent.get());
            let composer = state.composer_untracked();
            provide_context(state);
            session_settings_views(agent_ref, composer).1
        })
    }

    #[wasm_bindgen_test]
    async fn custom_acp_draft_and_active_agent_use_acp_settings_identity() {
        let draft_container = make_container();
        let draft_state = AppState::new();
        let host_id = "h-acp-draft";
        draft_state.selected_host_id.set(Some(host_id.to_owned()));
        crate::dispatch::prime_host_for_tests(&draft_state, host_id);
        draft_state.session_schemas.update(|map| {
            map.entry(host_id.to_owned()).or_default().insert(
                BackendKind::Kiro,
                SessionSchemaEntry::Ready {
                    schema: SessionSettingsSchema {
                        model_resolutions: Default::default(),
                        backend_kind: BackendKind::Kiro,
                        fields: vec![SessionSettingField {
                            key: "model".to_owned(),
                            label: "Model".to_owned(),
                            description: None,
                            field_type: SessionSettingFieldType::Toggle { default: false },
                            use_slider: false,
                            select_options_by_setting: None,
                        }],
                    },
                },
            );
        });
        draft_state.schemas_loaded_for_host.update(|map| {
            map.insert(host_id.to_owned(), true);
        });
        crate::actions::begin_new_chat_with_profile(
            &draft_state,
            LaunchProfile {
                id: LaunchProfileId("qa-acp".to_owned()),
                kind: LaunchProfileKind::Custom,
                label: "QA ACP".to_owned(),
                description: None,
                backend_kind: BackendKind::Kiro,
                session_settings: SessionSettingsValues::default(),
            },
            None,
        );
        let _draft_handle = mount_bar(&draft_container, draft_state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let draft_toggle = query(&draft_container, ".session-settings-toggle")
            .expect("custom ACP draft settings toggle");
        let draft_text = draft_toggle.text_content().unwrap_or_default();
        assert!(
            draft_text.contains("Session Settings (Kiro)") && !draft_text.contains("QA"),
            "custom Kiro draft must use the backend identity, not the profile label \"QA ACP\", \
             got: {draft_text}"
        );
        assert_eq!(
            draft_state
                .composer_untracked()
                .launch_profile_id
                .get_untracked(),
            Some(LaunchProfileId("qa-acp".to_owned())),
            "rendering the footer must not rewrite the selected launch profile"
        );

        let active_container = make_container();
        let active_state = make_state_for_backend("h-acp-active", "root", true, BackendKind::Kiro);
        let _active_handle = mount_bar(&active_container, active_state);
        for _ in 0..4 {
            next_tick().await;
        }

        let active_toggle = query(&active_container, ".session-settings-toggle")
            .expect("active ACP agent settings toggle");
        let active_text = active_toggle.text_content().unwrap_or_default();
        assert!(
            active_text.contains("Session Settings (Kiro)"),
            "active Kiro agent must retain the backend identity, got: {active_text}"
        );
    }

    fn select_field(
        key: &str,
        options: &[&str],
        nullable: bool,
        by_setting: Option<(&str, &[(&str, &[&str])])>,
    ) -> SessionSettingField {
        let to_options = |values: &[&str]| {
            values
                .iter()
                .map(|value| SelectOption {
                    value: (*value).to_owned(),
                    label: (*value).to_owned(),
                })
                .collect::<Vec<_>>()
        };
        SessionSettingField {
            key: key.to_owned(),
            label: key.to_owned(),
            description: None,
            field_type: SessionSettingFieldType::Select {
                options: to_options(options),
                default: (!nullable).then(|| options[0].to_owned()),
                nullable,
            },
            use_slider: false,
            select_options_by_setting: by_setting.map(|(setting_key, values)| {
                SelectOptionsBySetting {
                    setting_key: setting_key.to_owned(),
                    values: values
                        .iter()
                        .map(|(setting_value, options)| SelectOptionsForValue {
                            setting_value: (*setting_value).to_owned(),
                            options: to_options(options),
                        })
                        .collect(),
                }
            }),
        }
    }

    /// A multi-profile Hermes schema whose `work` profile offers `work_models`.
    fn hermes_profile_schema(work_models: &[&str]) -> SessionSettingsSchema {
        SessionSettingsSchema {
            model_resolutions: Default::default(),
            backend_kind: BackendKind::Hermes,
            fields: vec![
                select_field("profile", &["default", "work"], false, None),
                select_field(
                    "model",
                    &["default-model"],
                    true,
                    Some((
                        "profile",
                        &[("default", &["default-model"]), ("work", work_models)],
                    )),
                ),
                select_field("reasoning_effort", &["low", "high"], true, None),
            ],
        }
    }

    fn setting_row_select(container: &HtmlElement, label: &str) -> web_sys::HtmlSelectElement {
        let rows = container
            .query_selector_all(".session-setting-row")
            .unwrap();
        (0..rows.length())
            .filter_map(|index| rows.item(index)?.dyn_into::<HtmlElement>().ok())
            .find(|row| {
                row.query_selector(".session-setting-label")
                    .ok()
                    .flatten()
                    .and_then(|label| label.text_content())
                    .as_deref()
                    == Some(label)
            })
            .and_then(|row| row.query_selector("select").ok().flatten())
            .unwrap_or_else(|| panic!("a {label} select should render"))
            .dyn_into()
            .unwrap()
    }

    fn option_labels(select: &web_sys::HtmlSelectElement) -> Vec<(String, bool)> {
        let options = select.query_selector_all("option").unwrap();
        (0..options.length())
            .filter_map(|index| options.item(index))
            .map(|option| {
                let option: web_sys::HtmlOptionElement = option.dyn_into().unwrap();
                (option.text(), option.disabled())
            })
            .collect()
    }

    /// A running agent validates edits against the schema it started with, not
    /// the host's current catalog. Here the host catalog moved on after the
    /// agent started: the `work` profile dropped `old-model` and gained
    /// `new-model`. Rendering the host catalog made an unrelated edit silently
    /// reset the agent's model to Auto, and offered `new-model`, which the
    /// agent then rejected. The bar must render the agent's own schema.
    #[wasm_bindgen_test]
    async fn running_agent_settings_use_the_agent_schema_not_the_host_catalog() {
        install_send_stub();
        let container = make_container();
        let document = web_sys::window().unwrap().document().unwrap();
        let style = document.create_element("style").unwrap();
        style.set_text_content(Some(include_str!("../../styles.css")));
        container.append_child(&style).unwrap();
        let host_id = "h-agent-schema";
        let state = make_state_for_backend(host_id, "root", false, BackendKind::Hermes);
        state.session_schemas.update(|map| {
            map.entry(host_id.to_owned()).or_default().insert(
                BackendKind::Hermes,
                SessionSchemaEntry::Ready {
                    schema: hermes_profile_schema(&["new-model"]),
                },
            );
        });
        let mut values = SessionSettingsValues::default();
        for (key, value) in [
            ("profile", "work"),
            ("model", "old-model"),
            ("reasoning_effort", "low"),
        ] {
            values.0.insert(
                key.to_owned(),
                SessionSettingValue::String(value.to_owned()),
            );
        }
        dispatch_agent_session_settings(
            &state,
            "root",
            serde_json::json!({
                "values": values,
                "schema": hermes_profile_schema(&["old-model"]),
            }),
        );
        let _handle = mount_bar(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        query(&container, ".session-settings-toggle")
            .expect("the agent's settings header should render")
            .click();
        for _ in 0..4 {
            next_tick().await;
        }

        for label in ["profile", "model", "reasoning_effort"] {
            let select = setting_row_select(&container, label);
            let computed = web_sys::window()
                .unwrap()
                .get_computed_style(&select)
                .unwrap()
                .unwrap();
            assert_eq!(
                computed.get_property_value("appearance").unwrap(),
                "none",
                "session dropdowns must not use the glossy platform control"
            );
            let background_image = computed.get_property_value("background-image").unwrap();
            assert!(
                !background_image.is_empty() && background_image != "none",
                "flat session dropdowns still need a visible chevron"
            );
            assert!(
                computed
                    .get_property_value("padding-right")
                    .unwrap()
                    .trim_end_matches("px")
                    .parse::<f64>()
                    .unwrap()
                    >= 24.0,
                "setting values must not overlap the dropdown chevron"
            );
            assert!(select.get_bounding_client_rect().height() > 0.0);
        }

        let effort = setting_row_select(&container, "reasoning_effort");
        effort.set_value("high");
        effort
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        for _ in 0..4 {
            next_tick().await;
        }
        let frames = captured_frames("set_session_settings");
        assert_eq!(frames.len(), 1, "one edit is sent, got {frames:?}");
        let sent: SetSessionSettingsPayload =
            serde_json::from_str(&frames[0]).expect("parse sent SetSessionSettings");
        assert_eq!(
            sent.values.0.get("model"),
            Some(&SessionSettingValue::String("old-model".to_owned())),
            "an unrelated edit must not rewrite the agent's model"
        );
        assert_eq!(
            sent.values.0.get("reasoning_effort"),
            Some(&SessionSettingValue::String("high".to_owned())),
        );

        let model = setting_row_select(&container, "model");
        assert_eq!(
            model.value(),
            "old-model",
            "the model select must show the agent's model"
        );
        let model_options = option_labels(&model);
        assert!(
            model_options.contains(&("old-model".to_owned(), false)),
            "the agent's schema offers old-model, so it is an ordinary choice, got {model_options:?}"
        );
        assert!(
            !model_options.iter().any(|(label, _)| label == "new-model"),
            "a model the agent's schema does not offer must not be offered, got {model_options:?}"
        );
    }

    fn dispatch_agent_session_settings(
        state: &AppState,
        agent_id: &str,
        payload: serde_json::Value,
    ) {
        let envelope = Envelope::from_payload(
            StreamPath(format!("/agent/{agent_id}/inst")),
            FrameKind::SessionSettings,
            0,
            &payload,
        )
        .expect("envelope serialize");
        let host_id = state.selected_host_id.get_untracked().expect("host");
        crate::dispatch::dispatch_envelope(state, &host_id, envelope);
    }

    fn install_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
            "#,
        )
        .expect("install send stub");
    }

    fn captured_frames(kind: &str) -> Vec<String> {
        let script = format!(
            r#"
            (function() {{
                const out = [];
                for (const [cmd, args] of (window.__test_send_calls || [])) {{
                    if (cmd !== "send_host_line") continue;
                    const env = JSON.parse(JSON.parse(args).line);
                    if (env.kind === "{kind}") out.push(JSON.stringify(env.payload));
                }}
                return out.join("\n");
            }})()
            "#
        );
        let joined = js_sys::eval(&script)
            .expect("probe send calls")
            .as_string()
            .unwrap_or_default();
        if joined.is_empty() {
            Vec::new()
        } else {
            joined.lines().map(|line| line.to_owned()).collect()
        }
    }

    fn known_amount(input: u64, output: u64) -> TaskTokenUsageAmount {
        TaskTokenUsageAmount {
            partial_fields: Vec::new(),
            prompt_tokens: Some(input),
            total_tokens: input + output,
            input_tokens: Some(input),
            output_tokens: Some(output),
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn known_scope(input: u64, output: u64) -> TaskTokenUsageScope {
        TaskTokenUsageScope::Known {
            usage: Box::new(known_amount(input, output)),
        }
    }

    fn partial_scope(input: u64, output: u64) -> TaskTokenUsageScope {
        TaskTokenUsageScope::Partial {
            usage: Box::new(known_amount(input, output)),
            unavailable_count: 1,
            reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
        }
    }

    fn entry(
        agent_id: &str,
        parent_agent_id: Option<&str>,
        name: &str,
        depth: u32,
        tree_index: u32,
        usage: TaskTokenUsageScope,
    ) -> TaskTokenUsageEntry {
        TaskTokenUsageEntry {
            navigation: protocol::TaskUsageNavigation::LiveAgent,
            project_id: None,
            agent_id: AgentId(agent_id.to_owned()),
            session_id: None,
            parent_agent_id: parent_agent_id.map(|id| AgentId(id.to_owned())),
            parent_session_id: None,
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            model: Some("mock-model".to_owned()),
            depth,
            tree_index,
            usage,
        }
    }

    /// Server rollup for `root`: root + two children, one of which never
    /// reported usage, so the task total is Partial.
    fn partial_rollup(root: &str, root_in: u64, root_out: u64) -> TaskTokenUsagePayload {
        TaskTokenUsagePayload {
            helper_usage: Vec::new(),
            timing: Default::default(),
            backends: vec![protocol::TaskBackendUsage {
                analyzer_batch_count: 0,
                backend_kind: BackendKind::Claude,
                agent_count: 3,
                reporting_agent_count: 2,
                incomplete_agent_count: 1,
                total: TaskTokenUsageAggregate {
                    usage: known_amount(root_in + 1_000, root_out + 200),
                    status: TaskTokenUsageStatus::Partial {
                        unavailable_count: 1,
                        reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
                    },
                },
            }],
            root_agent_id: AgentId(root.to_owned()),
            root_session_id: None,
            total: TaskTokenUsageAggregate {
                usage: known_amount(root_in + 1_000, root_out + 200),
                status: TaskTokenUsageStatus::Partial {
                    unavailable_count: 1,
                    reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
                },
            },
            self_usage: known_scope(root_in, root_out),
            descendant_usage: TaskTokenUsageAggregate {
                usage: known_amount(1_000, 200),
                status: TaskTokenUsageStatus::Partial {
                    unavailable_count: 1,
                    reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
                },
            },
            descendant_count: 2,
            breakdown: vec![
                entry(root, None, "Root", 0, 0, known_scope(root_in, root_out)),
                entry(
                    "child-a",
                    Some(root),
                    "Child A",
                    1,
                    1,
                    known_scope(1_000, 200),
                ),
                entry(
                    "child-b",
                    Some(root),
                    "Child B",
                    1,
                    2,
                    TaskTokenUsageScope::Unavailable {
                        reason: TaskTokenUsageUnavailableReason::BackendDidNotReport,
                    },
                ),
            ],
        }
    }

    fn dispatch_task_usage(
        state: &AppState,
        host_id: &str,
        seq: u64,
        payload: &TaskTokenUsagePayload,
    ) {
        let envelope = Envelope::from_payload(
            StreamPath(format!("/host/{host_id}")),
            FrameKind::TaskTokenUsage,
            seq,
            payload,
        )
        .expect("envelope serialize");
        crate::dispatch::dispatch_envelope(state, host_id, envelope);
    }

    fn query(container: &HtmlElement, selector: &str) -> Option<HtmlElement> {
        container
            .query_selector(selector)
            .unwrap()
            .map(|el| el.dyn_into().unwrap())
    }

    /// A single-agent task (no descendants) still renders exactly one combined
    /// token display: the left-style `↑input ↓output` sourced from the server
    /// rollup total, with no `including N agents` suffix and no separate legacy
    /// cumulative badge. The settings toggle keeps its label.
    #[wasm_bindgen_test]
    async fn single_agent_shows_combined_usage_without_agent_suffix() {
        let container = make_container();
        let state = make_state_with_active_agent("h-task-solo", "root");
        let mut rollup = partial_rollup("root", 500, 100);
        rollup.descendant_count = 0;
        rollup.breakdown.truncate(1);
        rollup.total = TaskTokenUsageAggregate {
            usage: known_amount(500, 100),
            status: TaskTokenUsageStatus::Known,
        };
        dispatch_task_usage(&state, "h-task-solo", 0, &rollup);
        let _handle = mount_bar(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let toggle = query(&container, ".session-settings-toggle")
            .expect("settings toggle should render for the Ready schema");
        assert!(
            toggle
                .text_content()
                .unwrap_or_default()
                .contains("Session Settings (Claude)"),
            "existing header label must be unchanged"
        );

        let badge = query(&container, ".session-task-toggle")
            .expect("the combined token display renders for a single-agent task");
        let badge_text = badge.text_content().unwrap_or_default();
        assert!(
            badge_text.contains("\u{2191}500") && badge_text.contains("\u{2193}100"),
            "single-agent display must show the rollup total as ↑input ↓output, got: {badge_text}"
        );
        assert!(
            !badge_text.contains("including"),
            "a single-agent task must not append an agent-count suffix, got: {badge_text}"
        );
        assert!(
            !badge_text.contains('\u{03a3}'),
            "the combined display uses arrows, not a Σ total, got: {badge_text}"
        );

        // Exactly one token display: the legacy cumulative badge is gone.
        assert!(
            query(&container, ".session-settings-usage").is_none(),
            "the separate cumulative usage badge must be removed"
        );
        assert_eq!(
            container
                .query_selector_all(".session-task-toggle")
                .unwrap()
                .length(),
            1,
            "there must be exactly one task usage display, not two"
        );
    }

    async fn expand_usage(container: &HtmlElement, label: &str) {
        query(container, ".task-usage-expand")
            .expect("expand task usage")
            .click();
        next_tick().await;
        let tabs = container.query_selector_all("[role=tab]").unwrap();
        let tab = (0..tabs.length())
            .filter_map(|index| tabs.item(index))
            .find(|tab| tab.text_content().as_deref() == Some(label))
            .expect("expanded tab");
        let tab = tab.dyn_into::<HtmlElement>().unwrap();
        tab.focus().unwrap();
        tab.click();
        next_tick().await;
    }

    /// A multi-agent task renders the combined left-style total sourced from
    /// the server rollup (not summed chat rows), with an `including N agents`
    /// suffix (N = breakdown length) and a `partial` marker. Clicking opens the
    /// breakdown (rows in server order, indented by depth, unavailable reason
    /// spelled out), a live server update re-renders the open popover, and
    /// clicking again closes it.
    #[wasm_bindgen_test]
    async fn task_marker_and_breakdown_render_for_descendants() {
        let container = make_container();
        let state = make_state_with_active_agent("h-task-tree", "root");
        dispatch_task_usage(&state, "h-task-tree", 0, &partial_rollup("root", 500, 100));
        let _handle = mount_bar(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let badge = query(&container, ".session-task-toggle").expect("task badge should render");
        let badge_text = badge.text_content().unwrap_or_default();
        // Server total = self(500,100) + child-a(1000,200); child-b unavailable.
        assert!(
            badge_text.contains("\u{2191}1.5K") && badge_text.contains("\u{2193}300"),
            "badge must show the server task total as ↑input ↓output, got: {badge_text}"
        );
        assert!(
            !badge_text.contains('\u{03a3}'),
            "the combined display uses arrows, not a Σ total, got: {badge_text}"
        );
        assert!(
            badge_text.contains("including 3 agents"),
            "badge should carry the breakdown-length agent count, got: {badge_text}"
        );
        assert!(
            badge_text.contains("partial"),
            "a Partial rollup must be visibly marked, got: {badge_text}"
        );
        assert!(
            !badge_text.contains("unavailable"),
            "a Partial rollup with a known total must not look all-unavailable, got: {badge_text}"
        );
        assert!(
            query(&container, ".session-task-popover").is_none(),
            "breakdown must not be shown before click/hover"
        );
        assert_eq!(
            badge.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );

        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }
        let popover = query(&container, ".session-task-popover")
            .expect("clicking the badge should open the breakdown popover");
        assert_eq!(
            badge.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        let popover_text = popover.text_content().unwrap_or_default();
        assert!(
            popover_text.contains("Task usage"),
            "the compact task popup must aggregate by backend"
        );
        assert!(
            popover_text.contains("3 agents"),
            "popover title should count all breakdown rows, got: {popover_text}"
        );

        expand_usage(&container, "Agents").await;
        let rows = container
            .query_selector_all(".session-task-row")
            .expect("rows query");
        assert_eq!(rows.length(), 3, "one breakdown row per rollup entry");
        let row_texts: Vec<String> = (0..rows.length())
            .map(|i| rows.item(i).unwrap().text_content().unwrap_or_default())
            .collect();
        let root_pos = row_texts
            .iter()
            .position(|t| t.contains("Root"))
            .expect("root row rendered");
        let child_a_pos = row_texts
            .iter()
            .position(|t| t.contains("Child A"))
            .expect("child A row rendered");
        let child_b_pos = row_texts
            .iter()
            .position(|t| t.contains("Child B"))
            .expect("child B row rendered");
        assert!(
            root_pos < child_a_pos && child_a_pos < child_b_pos,
            "rows must render in server tree order, got: {row_texts:?}"
        );
        assert!(
            row_texts[child_b_pos].contains("backend did not report"),
            "unavailable rows must state the reason, got: {}",
            row_texts[child_b_pos]
        );
        assert!(
            row_texts[root_pos].contains("Claude"),
            "rows should show backend identity, got: {}",
            row_texts[root_pos]
        );

        // Children are indented under the root by depth.
        let names = container
            .query_selector_all(".session-task-row-name")
            .expect("names query");
        let name_x = |i: u32| -> f64 {
            names
                .item(i)
                .unwrap()
                .dyn_into::<web_sys::Element>()
                .unwrap()
                .get_bounding_client_rect()
                .x()
        };
        let row_display = web_sys::window()
            .unwrap()
            .get_computed_style(&query(&container, ".session-task-row").unwrap())
            .unwrap()
            .unwrap()
            .get_property_value("display")
            .unwrap();
        assert!(
            name_x(1) > name_x(0) && name_x(2) > name_x(0),
            "depth-1 rows must be indented past the root row; x={:?}, row display={row_display}",
            [name_x(0), name_x(1), name_x(2)]
        );

        // A live rollup update must re-render the open popover — no stale
        // snapshot of the breakdown.
        dispatch_task_usage(
            &state,
            "h-task-tree",
            1,
            &partial_rollup("root", 700_000, 100),
        );
        for _ in 0..4 {
            next_tick().await;
        }
        let popover = query(&container, ".session-task-popover")
            .expect("popover should stay open across a server update");
        let updated_text = popover.text_content().unwrap_or_default();
        assert!(
            updated_text.contains("↑700.0K"),
            "open popover must show the updated root row usage, got: {updated_text}"
        );
        let badge = query(&container, ".session-task-toggle").expect("badge still rendered");
        let updated_badge_text = badge.text_content().unwrap_or_default();
        assert!(
            updated_badge_text.contains("\u{2191}701.0K")
                && updated_badge_text.contains("\u{2193}300"),
            "badge must show the updated server total as ↑input ↓output, got: {updated_badge_text}"
        );

        let mut with_analyzer = partial_rollup("root", 700_000, 100);
        with_analyzer.total.usage = known_amount(701_400, 350);
        with_analyzer.helper_usage.push(protocol::LearningUsage {
            scope: protocol::SteeringScope::Host,
            batch_id: protocol::LearningChangeId("usage-analyzer-batch".to_owned()),
            origin_agent_ids: vec![AgentId("root".to_owned())],
            origin_session_ids: Vec::new(),
            input_ids: vec![protocol::HumanInputId("usage-human-input".to_owned())],
            analyzer: protocol::LearningAnalyzer {
                backend_kind: BackendKind::Codex,
                model: "source-model".to_owned(),
            },
            usage: known_scope(400, 50),
        });
        with_analyzer.backends.push(protocol::TaskBackendUsage {
            backend_kind: BackendKind::Codex,
            analyzer_batch_count: 1,
            agent_count: 0,
            reporting_agent_count: 0,
            incomplete_agent_count: 0,
            total: TaskTokenUsageAggregate {
                usage: known_amount(400, 50),
                status: TaskTokenUsageStatus::Known,
            },
        });
        dispatch_task_usage(&state, "h-task-tree", 2, &with_analyzer);
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            3,
            "Analyzer sources cannot fabricate additional agent rows"
        );
        let badge = query(&container, ".session-task-toggle").unwrap();
        assert!(
            badge.text_content().unwrap().contains("↑701.4K")
                && badge.text_content().unwrap().contains("↓350"),
            "The unchanged arrow footer projects the server total including its explicit analyzer sources"
        );
        query(&container, ".task-usage-back").unwrap().click();
        next_tick().await;
        let codex =
            query(&container, "article[aria-label=Codex]").expect("analyzer-only backend row");
        let text = codex.text_content().unwrap();
        assert!(
            text.contains("0 agents")
                && text.contains("1 analyzer batch")
                && text.contains("↑400")
                && text.contains("↓50"),
            "An analyzer-only backend keeps its usage without becoming an agent"
        );
        // Compact rows show the batch count; the allocation explanation is
        // disclosed in expanded Overview, not repeated in the compact popup.
        expand_usage(&container, "Overview").await;
        let analyzer_coverage = query(&container, ".task-timing-coverage").unwrap();
        query(&analyzer_coverage, "summary").unwrap().click();
        next_tick().await;
        assert!(
            analyzer_coverage.inner_text().contains(
                "Shared batches are source scopes, not per-task allocations or additional agents"
            ),
            "the disclosed analyzer explanation must distinguish source scopes from task allocations"
        );
        query(&container, ".task-usage-back").unwrap().click();
        next_tick().await;
        let mut mixed = partial_rollup("root", 2_000, 200);
        let mut codex_amount = known_amount(2_000, 200);
        codex_amount.input_tokens = Some(1_000);
        codex_amount.cached_prompt_tokens = Some(1_000);
        codex_amount.reasoning_tokens = Some(50);
        let mut claude_amount = known_amount(1_000, 100);
        claude_amount.input_tokens = Some(300);
        claude_amount.cached_prompt_tokens = Some(700);
        claude_amount.reasoning_tokens = Some(20);
        let fields = vec![
            TaskTokenUsageField::Prompt,
            TaskTokenUsageField::Input,
            TaskTokenUsageField::Output,
            TaskTokenUsageField::CachedPrompt,
            TaskTokenUsageField::Reasoning,
        ];
        let mut claude_subtotal = claude_amount.clone();
        claude_subtotal.total_tokens += 47;
        claude_subtotal.partial_fields = fields.clone();
        let mut total = known_amount(3_000, 300);
        total.total_tokens += 47;
        total.input_tokens = Some(1_300);
        total.cached_prompt_tokens = Some(1_700);
        total.reasoning_tokens = Some(70);
        total.partial_fields = fields;
        mixed.total = TaskTokenUsageAggregate {
            usage: total,
            status: TaskTokenUsageStatus::Known,
        };
        mixed.backends = vec![
            protocol::TaskBackendUsage {
                backend_kind: BackendKind::Codex,
                analyzer_batch_count: 0,
                agent_count: 1,
                reporting_agent_count: 1,
                incomplete_agent_count: 0,
                total: TaskTokenUsageAggregate {
                    usage: codex_amount.clone(),
                    status: TaskTokenUsageStatus::Known,
                },
            },
            protocol::TaskBackendUsage {
                backend_kind: BackendKind::Claude,
                analyzer_batch_count: 0,
                agent_count: 2,
                reporting_agent_count: 2,
                incomplete_agent_count: 0,
                total: TaskTokenUsageAggregate {
                    usage: claude_subtotal,
                    status: TaskTokenUsageStatus::Known,
                },
            },
        ];
        mixed.breakdown[0].backend_kind = BackendKind::Codex;
        mixed.breakdown[0].usage = TaskTokenUsageScope::Known {
            usage: Box::new(codex_amount),
        };
        mixed.breakdown[1].usage = TaskTokenUsageScope::Known {
            usage: Box::new(claude_amount),
        };
        mixed.breakdown[2].usage = TaskTokenUsageScope::Known {
            usage: Box::new(TaskTokenUsageAmount::total_only(47)),
        };
        dispatch_task_usage(&state, "h-task-tree", 3, &mixed);
        next_tick().await;
        let text = query(&container, ".session-task-toggle")
            .unwrap()
            .text_content()
            .unwrap();
        assert!(
            text.contains("↑≥3.0K (cached ≥1.7K)")
                && text.contains("↓≥300 (reasoning ≥70)")
                && text.contains("partial"),
            "known component subtotals stay visible with incomplete coverage"
        );
        let codex = query(&container, "article[aria-label=Codex]")
            .unwrap()
            .text_content()
            .unwrap();
        assert!(
            codex.contains("↑2.0K (cached 1.0K)")
                && codex.contains("↓200 (reasoning 50)")
                && !codex.contains("≥")
        );
        let claude = query(&container, "article[aria-label=Claude]")
            .unwrap()
            .text_content()
            .unwrap();
        assert!(
            claude.contains("↑≥1.0K (cached ≥700)")
                && claude.contains("↓≥100 (reasoning ≥20)")
                && claude.contains("partial breakdown")
        );
        expand_usage(&container, "Agents").await;
        let rows = container.query_selector_all(".session-task-row").unwrap();
        let total_only = rows.item(2).unwrap().text_content().unwrap();
        assert!(
            total_only.contains("↑— (cached —)")
                && total_only.contains("↓— (reasoning —)")
                && total_only.contains("Reported combined: 47"),
            "unknown split components cannot become zero or borrow another agent's usage"
        );
        let badge = query(&container, ".session-task-toggle").expect("badge still rendered");
        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_none(),
            "clicking the badge again must close the breakdown"
        );
    }

    #[wasm_bindgen_test]
    async fn task_usage_eight_backends_tabs_filters_limits_and_host_bound_navigation() {
        install_send_stub();
        let container = make_container();
        let state = make_state_with_active_agent("h-task-expanded", "root");
        let kinds = [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Antigravity,
            BackendKind::Grok,
            BackendKind::Hermes,
            BackendKind::Kiro,
            BackendKind::Muse,
            BackendKind::Opencode,
        ];
        let mut rollup = partial_rollup("root", 1_000, 100);
        let mut amount = known_amount(1_000, 100);
        amount.cached_prompt_tokens = Some(0);
        rollup.breakdown = (0..87)
            .map(|index| {
                let mut row = entry(
                    if index == 0 { "root" } else { "unused" },
                    (index != 0).then_some("root"),
                    &format!("Worker {index}"),
                    u32::from(index != 0),
                    index,
                    TaskTokenUsageScope::Known {
                        usage: Box::new(amount.clone()),
                    },
                );
                if index != 0 {
                    row.agent_id = AgentId(format!("worker-{index}"));
                }
                row.backend_kind = kinds[index as usize % kinds.len()];
                row
            })
            .collect();
        rollup.breakdown[85].usage = TaskTokenUsageScope::Known {
            usage: Box::new(TaskTokenUsageAmount::total_only(42)),
        };
        rollup.breakdown[86].usage = TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::BackendDidNotReport,
        };
        rollup.total.usage = TaskTokenUsageAmount {
            partial_fields: Vec::new(),
            prompt_tokens: Some(86_000),
            total_tokens: 94_600,
            input_tokens: Some(86_000),
            output_tokens: Some(8_600),
            cached_prompt_tokens: Some(0),
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        };
        rollup.breakdown[86].navigation = protocol::TaskUsageNavigation::SavedSession {
            session_id: protocol::SessionId("saved-usage-session".to_owned()),
        };
        rollup.descendant_count = 86;
        rollup.backends = kinds
            .iter()
            .map(|kind| {
                let count = rollup
                    .breakdown
                    .iter()
                    .filter(|entry| entry.backend_kind == *kind)
                    .count() as u32;
                let missing = u32::from(*kind == kinds[86 % kinds.len()]);
                let mut usage = known_amount(
                    u64::from(count - missing) * 1_000,
                    u64::from(count - missing) * 100,
                );
                usage.cached_prompt_tokens = Some(0);
                protocol::TaskBackendUsage {
                    analyzer_batch_count: 0,
                    backend_kind: *kind,
                    agent_count: count,
                    reporting_agent_count: count - missing,
                    incomplete_agent_count: missing,
                    total: TaskTokenUsageAggregate {
                        usage,
                        status: if missing == 0 {
                            TaskTokenUsageStatus::Known
                        } else {
                            rollup.total.status.clone()
                        },
                    },
                }
            })
            .collect();
        rollup.timing = protocol::TaskTimingSummary {
            covered_agent_count: 1,
            unobserved_agent_count: 86,
            observed_start_ms: Some(1_000),
            observed_end_ms: Some(6_000),
            summed_tool_ms: 5_000,
            summed_dependency_wait_ms: 2_000,
            command_count: 1,
            classified_tool_count: 1,
            spans: vec![protocol::TaskTimingEntry {
                agent_id: AgentId("root".to_owned()),
                span: protocol::TaskTimingSpan {
                    tool_call_id: "check-observed".to_owned(),
                    category: protocol::TaskTimingCategory::Command {
                        command: "./dev.sh check".to_owned(),
                        check: true,
                    },
                    started_at_ms: 1_000,
                    observed_through_ms: 6_000,
                    observed_duration_ms: 5_000,
                    state: protocol::TaskTimingState::Completed,
                },
            }],
            ..Default::default()
        };
        rollup.timing.spans.push(protocol::TaskTimingEntry {
            agent_id: AgentId("root".to_owned()),
            span: protocol::TaskTimingSpan {
                tool_call_id: "overlapping-await".to_owned(),
                category: protocol::TaskTimingCategory::DependencyWait {
                    agent_ids: vec![AgentId("child-1".to_owned())],
                    omitted_link_count: Some(0),
                    targets_observed: true,
                },
                started_at_ms: 2_000,
                observed_through_ms: 4_000,
                observed_duration_ms: 2_000,
                state: protocol::TaskTimingState::Completed,
            },
        });
        dispatch_task_usage(&state, "h-task-expanded", 0, &rollup);
        dispatch_capacity(&state, "h-task-expanded", 1, 0);
        let mut claude =
            state.backend_capacity.get_untracked()["h-task-expanded"][&BackendKind::Claude].clone();
        let protocol::BackendCapacityState::Known { report } = &mut claude.state else {
            unreachable!()
        };
        report.coverage = protocol::CapacityCoverage::AllVendorBuckets;
        report.buckets[0].id = protocol::CapacityBucketId::Claude {
            limit: protocol::ClaudeLimitType::FiveHour,
        };
        report.buckets[0].scope = protocol::CapacityScope::Account;
        report.buckets[0].window = protocol::CapacityWindow::Rolling {
            duration_minutes: 300,
        };
        let mut week = report.buckets[0].clone();
        week.id = protocol::CapacityBucketId::Claude {
            limit: protocol::ClaudeLimitType::SevenDay,
        };
        week.label = "weekly".to_owned();
        week.window = protocol::CapacityWindow::Rolling {
            duration_minutes: 10_080,
        };
        week.measure = protocol::CapacityMeasure::UsedPercent {
            used_percent: 97,
            remaining_percent: 3,
            provenance: protocol::ValueProvenance {
                vendor_reported: true,
            },
        };
        report.buckets.push(week);
        let mut gemini = claude.clone();
        gemini.backend_kind = BackendKind::Antigravity;
        let protocol::BackendCapacityState::Known { report } = &mut gemini.state else {
            unreachable!()
        };
        report.source = protocol::CapacitySource::AntigravityUsageCommand;
        for (index, bucket) in report.buckets.iter_mut().enumerate() {
            bucket.id = protocol::CapacityBucketId::Antigravity {
                bucket: format!("gemini-{index}"),
            };
            bucket.scope = protocol::CapacityScope::ModelFamily {
                name: "Gemini".to_owned(),
            };
            bucket.measure = protocol::CapacityMeasure::UsedPercent {
                used_percent: 2,
                remaining_percent: 98,
                provenance: protocol::ValueProvenance {
                    vendor_reported: false,
                },
            };
        }
        let mut argon = report.buckets[1].clone();
        argon.id = protocol::CapacityBucketId::Antigravity {
            bucket: "argon-weekly".to_owned(),
        };
        argon.scope = protocol::CapacityScope::ModelFamily {
            name: "Gemini 4 Argon".to_owned(),
        };
        argon.measure = protocol::CapacityMeasure::UsedPercent {
            used_percent: 100,
            remaining_percent: 0,
            provenance: protocol::ValueProvenance {
                vendor_reported: false,
            },
        };
        report.buckets.push(argon);
        let mut third_party = report.buckets[1].clone();
        third_party.id = protocol::CapacityBucketId::Antigravity {
            bucket: "third-party-weekly".to_owned(),
        };
        third_party.scope = protocol::CapacityScope::ModelFamily {
            name: "Claude and GPT models".to_owned(),
        };
        report.buckets.push(third_party);
        let mut unknown = report.buckets[0].clone();
        unknown.id = protocol::CapacityBucketId::Antigravity {
            bucket: "unreported-magnitude".to_owned(),
        };
        unknown.scope = protocol::CapacityScope::ModelFamily {
            name: "Unknown magnitude".to_owned(),
        };
        unknown.measure = protocol::CapacityMeasure::ReportedWithoutMagnitude;
        report.buckets.push(unknown);
        gemini.state = protocol::BackendCapacityState::Stale {
            report: report.clone(),
            stale_since_ms: 3_600_000,
            last_error: None,
        };
        gemini.freshness = protocol::CapacityFreshness::Stale {
            age_ms: 7_200_000,
            threshold_ms: 3_600_000,
        };
        let protocol::BackendCapacityState::Known { report } = &mut claude.state else {
            unreachable!()
        };
        let mut family = report.buckets[1].clone();
        family.id = protocol::CapacityBucketId::ClaudeModel {
            name: "Fable".to_owned(),
        };
        family.label = "Fable limit".to_owned();
        report.buckets.push(family);
        let mut codex = claude.clone();
        codex.backend_kind = BackendKind::Codex;
        let protocol::BackendCapacityState::Known { report } = &mut codex.state else {
            unreachable!()
        };
        report.source = protocol::CapacitySource::CodexAccountRateLimitsUpdated;
        report.buckets.truncate(1);
        report.buckets[0].id = protocol::CapacityBucketId::Codex {
            slot: protocol::CodexLimitSlot::Primary,
        };
        let mut credits = report.buckets[0].clone();
        credits.id = protocol::CapacityBucketId::Codex {
            slot: protocol::CodexLimitSlot::Credits,
        };
        credits.label = "credits".to_owned();
        credits.window = protocol::CapacityWindow::NotReported;
        credits.measure = protocol::CapacityMeasure::Credits {
            has_credits: true,
            unlimited: true,
            balance: Some("12345.6789".to_owned()),
        };
        report.buckets.push(credits);
        let capacity = Envelope::from_payload(
            StreamPath("/host/h-task-expanded".to_owned()),
            FrameKind::BackendCapacity,
            2,
            &protocol::BackendCapacityPayload {
                snapshots: vec![claude, gemini, codex],
            },
        )
        .unwrap();
        crate::dispatch::dispatch_envelope(&state, "h-task-expanded", capacity);
        let fixed_state = state.clone();
        let _handle = mount_to(container.clone(), move || {
            let composer = fixed_state.composer_untracked();
            provide_context(fixed_state.clone());
            let agent_ref = Signal::derive(|| {
                Some(ActiveAgentRef {
                    host_id: "h-task-expanded".to_owned(),
                    agent_id: AgentId("root".to_owned()),
                })
            });
            session_settings_views(agent_ref, composer).1
        });
        next_tick().await;
        let badge = query(&container, ".session-task-toggle").unwrap();
        let original = badge.text_content().unwrap();
        let footer_before = badge.get_bounding_client_rect();
        assert!(
            original.contains("↑86.0K")
                && original.contains("including 87 agents")
                && original.contains("partial")
        );
        badge.click();
        next_tick().await;
        let popup_rect = query(&container, ".session-task-popover")
            .unwrap()
            .get_bounding_client_rect();
        assert!(
            popup_rect.left() >= 0.0
                && popup_rect.right()
                    <= web_sys::window()
                        .unwrap()
                        .inner_width()
                        .unwrap()
                        .as_f64()
                        .unwrap(),
            "the compact popup must fit the viewport even when its badge is in a narrow pane"
        );
        assert_eq!(
            container
                .query_selector_all(".task-backend-row")
                .unwrap()
                .length(),
            8,
            "one flat row per backend, never 87 agent cards"
        );
        let visible =
            js_sys::Function::new_with_args("element", "return element.checkVisibility();");
        let tracks = container.query_selector_all(".task-limit-track").unwrap();
        let visible_tracks = (0..tracks.length())
            .filter(|index| {
                visible
                    .call1(&wasm_bindgen::JsValue::NULL, &tracks.item(*index).unwrap())
                    .unwrap()
                    .as_bool()
                    == Some(true)
            })
            .count();
        assert_eq!(
            visible_tracks, 3,
            "only one priority quota per reporting backend is visible initially"
        );
        let claude = query(&container, "article[aria-label=Claude]").unwrap();
        let gemini = query(&container, "article[aria-label=Antigravity]").unwrap();
        assert!(
            query(&claude, ".task-limit-primary")
                .unwrap()
                .inner_text()
                .contains("account · 7d")
        );
        assert!(
            query(&claude, ".task-limit-primary")
                .unwrap()
                .inner_text()
                .contains("97%")
        );
        assert!(
            query(&gemini, ".task-limit-primary")
                .unwrap()
                .inner_text()
                .contains("Gemini 4 Argon · 7d")
        );
        let estimate = query(&gemini, ".task-limit-primary [role=progressbar]").unwrap();
        assert_eq!(
            estimate.get_attribute("aria-valuenow").as_deref(),
            Some("100")
        );
        assert!(
            estimate
                .get_attribute("aria-valuetext")
                .unwrap()
                .contains("estimated, derived")
        );
        assert_eq!(
            query(&gemini, ".task-limit-others summary")
                .unwrap()
                .inner_text(),
            "+4"
        );
        let disclosure = query(&gemini, ".task-limit-others summary").unwrap();
        disclosure.focus().unwrap();
        disclosure.click();
        query(&claude, ".task-limit-others summary")
            .unwrap()
            .click();
        query(
            &container,
            "article[aria-label=Codex] .task-limit-others summary",
        )
        .unwrap()
        .click();
        next_tick().await;
        let disclosed = query(&gemini, ".task-limit-buckets").unwrap();
        assert!(
            disclosed.inner_text().contains("Gemini · 5h")
                && disclosed.inner_text().contains("Gemini · 7d")
                && disclosed
                    .inner_text()
                    .contains("Claude and GPT models · 7d")
                && disclosed.inner_text().contains("Unknown magnitude")
                && disclosed.inner_text().contains("Usage not reported")
        );
        assert_eq!(
            gemini
                .query_selector_all("[role=progressbar]")
                .unwrap()
                .length(),
            4,
            "unknown magnitudes never get fake zero bars"
        );
        assert!(claude.inner_text().contains("Fable · 7d"));
        let codex = query(&container, "article[aria-label=Codex]").unwrap();
        assert!(
            codex.inner_text().contains("Unlimited credits")
                && !codex.text_content().unwrap().contains("12345.6789")
        );
        let stale = query(&gemini, ".task-limit-context [role=note]").unwrap();
        assert_eq!(stale.get_attribute("tabindex").as_deref(), Some("0"));
        assert!(
            stale
                .get_attribute("aria-label")
                .unwrap()
                .contains("past the 1h freshness threshold")
        );
        let label = query(&claude, ".task-limit-primary .task-limit-label")
            .unwrap()
            .get_bounding_client_rect();
        let track = query(&claude, ".task-limit-primary .task-limit-track")
            .unwrap()
            .get_bounding_client_rect();
        assert!(
            label.bottom() <= track.top(),
            "plain scope/window label sits above the quiet rounded track"
        );
        let first_track = query(&claude, ".task-limit-primary .task-limit-track")
            .unwrap()
            .get_bounding_client_rect();
        let second_track = query(&gemini, ".task-limit-primary .task-limit-track")
            .unwrap()
            .get_bounding_client_rect();
        assert_eq!(
            first_track.left(),
            second_track.left(),
            "priority bars align across backend rows"
        );
        let icon = claude
            .query_selector(".launch-backend-icon")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        assert!(
            icon.width() <= 16.0 && icon.height() <= 16.0,
            "reuse tiny authentic provider artwork"
        );
        assert_eq!(
            badge.text_content().unwrap(),
            original,
            "popup disclosure does not replace or change footer figures"
        );
        let footer_after = badge.get_bounding_client_rect();
        assert_eq!(
            (footer_before.left(), footer_before.top()),
            (footer_after.left(), footer_after.top())
        );
        let mut refreshed = state.backend_capacity.get_untracked()["h-task-expanded"]
            [&BackendKind::Antigravity]
            .clone();
        let protocol::BackendCapacityState::Stale { report, .. } = &mut refreshed.state else {
            unreachable!()
        };
        report.buckets[2].measure = protocol::CapacityMeasure::UsedPercent {
            used_percent: 99,
            remaining_percent: 1,
            provenance: protocol::ValueProvenance {
                vendor_reported: false,
            },
        };
        let frame = Envelope::from_payload(
            StreamPath("/host/h-task-expanded".to_owned()),
            FrameKind::BackendCapacity,
            3,
            &protocol::BackendCapacityPayload {
                snapshots: vec![refreshed],
            },
        )
        .unwrap();
        crate::dispatch::dispatch_envelope(&state, "h-task-expanded", frame);
        next_tick().await;
        assert!(
            disclosure.is_connected() && disclosure.parent_element().unwrap().has_attribute("open"),
            "live quota updates preserve open disclosure and its focusable summary"
        );
        assert!(
            query(&gemini, ".task-limit-primary")
                .unwrap()
                .inner_text()
                .contains("~99%")
        );
        let summary_node = query(&container, ".task-backend-drilldown").unwrap();
        let mut update = rollup.clone();
        update.backends[0].total.usage.input_tokens = Some(999_000);
        update.backends[0].total.usage.prompt_tokens = Some(999_000);
        dispatch_task_usage(&state, "h-task-expanded", 4, &update);
        next_tick().await;
        assert!(
            query(&container, "article[aria-label=Claude]")
                .unwrap()
                .text_content()
                .unwrap()
                .contains("↑999.0K")
        );
        assert!(
            summary_node.is_connected(),
            "same-key rows stay mounted on live updates"
        );
        expand_usage(&container, "Overview").await;
        let document = web_sys::window().unwrap().document().unwrap();
        assert_eq!(
            document.active_element().unwrap().tag_name(),
            "BUTTON",
            "selecting a tab preserves keyboard focus"
        );
        let tabs = container.query_selector_all("[role=tab]").unwrap();
        tabs.item(1)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        let text = query(&container, ".task-time-view")
            .unwrap()
            .text_content()
            .unwrap();
        assert!(
            text.contains("5.0s")
                && text.contains("1/87 agents")
                && text.contains("Process lifetime: unsupported")
        );
        assert_eq!(
            container
                .query_selector_all(".task-timing-agent")
                .unwrap()
                .length(),
            1,
            "overlapping operations share one agent row"
        );
        let segments = container
            .query_selector_all(".task-timing-segment")
            .unwrap();
        assert_eq!(segments.length(), 2);
        let command = segments.item(0).unwrap().dyn_into::<HtmlElement>().unwrap();
        let wait = segments.item(1).unwrap().dyn_into::<HtmlElement>().unwrap();
        let command_rect = command.get_bounding_client_rect();
        let wait_rect = wait.get_bounding_client_rect();
        assert!(
            command_rect.bottom() <= wait_rect.top() && command_rect.left() < wait_rect.right(),
            "parallel intervals remain individually visible in micro-lanes"
        );
        let operation = query(&container, ".task-timing-event").unwrap();
        // Chrome returned a 23.453125px layout box inside closed native details;
        // the native capture shows no operation list. Test visibility and input,
        // not layout retained for content hidden by the browser's details slot.
        let operation_visible =
            js_sys::Function::new_with_args("element", "return element.checkVisibility();");
        assert_eq!(
            operation_visible
                .call1(&wasm_bindgen::JsValue::NULL, &operation)
                .unwrap()
                .as_bool(),
            Some(false),
            "individual operations are not visible when collapsed"
        );
        let expansion = query(&container, ".task-timing-events summary").unwrap();
        assert!(!expansion.parent_element().unwrap().has_attribute("open"));
        expansion.focus().unwrap();
        operation.focus().unwrap();
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&expansion)),
            "collapsed operations cannot take keyboard focus"
        );
        let operation_rect = operation.get_bounding_client_rect();
        assert!(
            !document
                .element_from_point(
                    (operation_rect.left() + operation_rect.width() / 2.0) as f32,
                    (operation_rect.top() + operation_rect.height() / 2.0) as f32,
                )
                .is_some_and(|hit| operation.contains(Some(&hit))),
            "collapsed operations cannot intercept pointer interaction"
        );
        expansion.click();
        next_tick().await;
        assert!(
            operation_visible
                .call1(&wasm_bindgen::JsValue::NULL, &operation)
                .unwrap()
                .as_bool()
                == Some(true)
                && operation.get_bounding_client_rect().height() > 0.0,
            "expanding reveals the original individual-operation list"
        );
        operation.focus().unwrap();
        update.timing.spans[0].span.observed_duration_ms = 5_100;
        update.timing.spans[0].span.observed_through_ms = 6_100;
        update.timing.observed_end_ms = Some(6_100);
        update.timing.summed_tool_ms = 5_100;
        dispatch_task_usage(&state, "h-task-expanded", 5, &update);
        next_tick().await;
        assert!(
            expansion.is_connected() && operation.is_connected(),
            "same-agent timing updates retain expanded rows and operation buttons"
        );
        assert!(expansion.parent_element().unwrap().has_attribute("open"));
        assert!(
            operation.text_content().unwrap().contains("5.1s"),
            "retained operations update their observed durations"
        );
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&operation)),
            "live rollups preserve keyboard focus inside the expanded operation list"
        );
        expansion.click();
        wait.focus().unwrap();
        next_tick().await;
        let detail = query(&container, ".task-timing-detail").unwrap();
        assert!(
            detail
                .text_content()
                .unwrap()
                .contains("Observed causal links")
        );
        assert!(detail.text_content().unwrap().contains("Start 2000 ms"));
        command.click();
        next_tick().await;
        assert!(
            query(&container, ".task-timing-detail")
                .unwrap()
                .text_content()
                .unwrap()
                .contains("not evidence of background process exit")
        );
        tabs.item(2)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            87
        );
        let input = query(&container, "input[aria-label='Filter agents']")
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        input.set_value("Worker 85");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert!(
            query(&container, ".session-task-row")
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Reported combined: 42 · input/output unreported"),
            "details preserve a reported total without inventing input/output splits or replacing the footer"
        );
        input.set_value("Worker 86");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            1
        );
        assert!(
            query(&container, ".session-task-row")
                .unwrap()
                .text_content()
                .unwrap()
                .contains("backend did not report")
        );
        input.set_value("");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        query(&container, ".task-tree-toggle").unwrap().click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            1,
            "collapse hides descendants without changing scope totals"
        );
        let buttons = container
            .query_selector_all(".task-agent-controls button")
            .unwrap();
        buttons
            .item(1)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            87,
            "flat view ignores tree collapse"
        );
        buttons
            .item(0)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            1
        );
        query(&container, ".task-usage-close").unwrap().click();
        next_tick().await;
        assert!(
            document
                .active_element()
                .unwrap()
                .is_same_node(Some(&badge))
        );
        assert_eq!(
            badge.text_content().unwrap(),
            original,
            "expanded views leave the footer untouched"
        );
        badge.click();
        next_tick().await;
        query(&container, ".task-backend-drilldown")
            .unwrap()
            .click();
        next_tick().await;
        assert!(
            query(&container, "[role=tab][aria-selected=true]")
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Agents")
        );
        assert_eq!(
            container
                .query_selector_all(".session-task-row")
                .unwrap()
                .length(),
            11,
            "backend drilldown filters own-usage rows"
        );
        state.agents.update(|agents| {
            let mut target = agents[0].clone();
            target.agent_id = AgentId("worker-8".to_owned());
            target.name = "Worker 8".to_owned();
            target.parent_agent_id = Some(AgentId("root".to_owned()));
            let mut other = target.clone();
            other.host_id = "different-host".to_owned();
            other.name = "Wrong host target".to_owned();
            agents.insert(0, other);
            agents.push(target);
        });
        let destination = make_container();
        let destination_state = state.clone();
        let _destination_handle = mount_to(destination.clone(), move || {
            provide_context(destination_state.clone());
            view! { <crate::components::center_zone::CenterZone /> }
        });
        next_tick().await;
        assert!(!destination.text_content().unwrap().contains("Worker 8"));
        container
            .query_selector_all(".task-agent-open")
            .unwrap()
            .item(1)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        next_tick().await;
        assert_eq!(
            query(&destination, "[role=tab][aria-selected=true] .tab-label")
                .expect("selected destination tab")
                .text_content()
                .as_deref(),
            Some("Worker 8")
        );
        assert_eq!(
            query(
                &destination,
                "[role=tabpanel]:not([style*='display: none']) .chat-agent-name"
            )
            .expect("visible destination chat header")
            .text_content()
            .as_deref(),
            Some("Worker 8"),
            "navigation must select the exact agent on the source host, not an ID match on another host"
        );
        query(&container, ".session-task-toggle").unwrap().click();
        next_tick().await;
        expand_usage(&container, "Agents").await;
        let filter = query(&container, "input[aria-label='Filter agents']")
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        filter.set_value("Worker 86");
        filter
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;
        query(&container, ".task-agent-open").unwrap().click();
        next_tick().await;
        let sent = captured_frames("spawn_agent");
        assert_eq!(
            sent.len(),
            1,
            "opening a closed row sends one explicit resume"
        );
        let resume: protocol::SpawnAgentPayload = serde_json::from_str(&sent[0]).unwrap();
        assert!(
            matches!(resume.params, protocol::SpawnAgentParams::Resume { session_id, .. } if session_id.0 == "saved-usage-session")
        );
        assert_eq!(
            resume.parent_agent_id,
            Some(AgentId("root".to_owned())),
            "saved-child resume must retain its server-emitted parent"
        );
        let host = js_sys::eval("JSON.parse(window.__test_send_calls.filter(([cmd]) => cmd === 'send_host_line').at(-1)[1]).hostId").unwrap();
        assert_eq!(host.as_string().as_deref(), Some("h-task-expanded"));
    }

    /// A row-level Partial scope is not an all-unavailable row: it carries the
    /// reported number and a visible unreported marker in the breakdown.
    #[wasm_bindgen_test]
    async fn task_partial_breakdown_row_shows_number_and_unreported_marker() {
        let container = make_container();
        let state = make_state_with_active_agent("h-task-row-partial", "root");
        let mut rollup = partial_rollup("root", 500, 100);
        rollup.breakdown[1].usage = partial_scope(1_000, 200);
        rollup.total.status = TaskTokenUsageStatus::Partial {
            unavailable_count: 2,
            reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
        };
        rollup.descendant_usage.status = TaskTokenUsageStatus::Partial {
            unavailable_count: 2,
            reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
        };
        dispatch_task_usage(&state, "h-task-row-partial", 0, &rollup);
        let _handle = mount_bar(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        let badge = query(&container, ".session-task-toggle").expect("task badge should render");
        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }
        expand_usage(&container, "Agents").await;
        let rows = container
            .query_selector_all(".session-task-row")
            .expect("rows query");
        let row_texts: Vec<String> = (0..rows.length())
            .map(|i| rows.item(i).unwrap().text_content().unwrap_or_default())
            .collect();
        let child_a_text = row_texts
            .iter()
            .find(|text| text.contains("Child A"))
            .expect("child A row rendered");

        assert!(
            child_a_text.contains("\u{2191}1.0K") && child_a_text.contains("\u{2193}200"),
            "partial row must show the reported token figure, got: {child_a_text}"
        );
        assert!(
            child_a_text.contains("partial") && child_a_text.contains("1 unreported"),
            "partial row must show an unreported marker, got: {child_a_text}"
        );
        assert!(
            !child_a_text.contains("usage unavailable")
                && !child_a_text.contains("backend did not report"),
            "partial row must not render as all-unavailable, got: {child_a_text}"
        );
    }

    /// A rollup whose total is Unavailable (no agent reported; the server
    /// sends `total_tokens: 0` with absent splits) must say so explicitly —
    /// the badge shows only the unavailable marker, never a fabricated Σ0.
    #[wasm_bindgen_test]
    async fn task_unavailable_rollup_is_marked() {
        let container = make_container();
        let state = make_state_with_active_agent("h-task-unavail", "root");
        let mut rollup = partial_rollup("root", 0, 0);
        rollup.total = TaskTokenUsageAggregate {
            usage: TaskTokenUsageAmount::total_only(0),
            status: TaskTokenUsageStatus::Unavailable {
                unavailable_count: 3,
                reasons: vec![TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted],
            },
        };
        rollup.backends[0].total = rollup.total.clone();
        for entry in &mut rollup.breakdown {
            entry.usage = TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted,
            };
        }
        dispatch_task_usage(&state, "h-task-unavail", 0, &rollup);
        let _handle = mount_bar(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let badge = query(&container, ".session-task-toggle").expect("task badge should render");
        let badge_text = badge.text_content().unwrap_or_default();
        assert!(
            badge_text.contains("unavailable"),
            "an Unavailable rollup must be visibly marked, got: {badge_text}"
        );
        assert!(
            !badge_text.contains('Σ') && !badge_text.contains('0'),
            "an all-unavailable total must not render as a real zero, got: {badge_text}"
        );
        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }
        let popover = query(&container, ".session-task-popover").expect("popover opens");
        let popover_text = popover.text_content().unwrap_or_default();
        assert!(
            popover_text.contains("no completed turns"),
            "rows must spell out the unavailable reason, got: {popover_text}"
        );
        rollup.total.status = TaskTokenUsageStatus::Unavailable {
            unavailable_count: 3,
            reasons: vec![TaskTokenUsageUnavailableReason::SharedSessionAccounting],
        };
        rollup.backends[0].total = rollup.total.clone();
        for entry in &mut rollup.breakdown {
            entry.usage = TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::SharedSessionAccounting,
            };
        }
        dispatch_task_usage(&state, "h-task-unavail", 1, &rollup);
        next_tick().await;
        let text = popover.text_content().unwrap_or_default();
        assert!(
            text.contains("shared session accounting cannot be attributed"),
            "the live backend row must name shared-source ambiguity, not display a copied or zero token total"
        );
        assert!(
            !text.contains("no completed turns"),
            "same-key rows must replace stale coverage reasons"
        );
        rollup.total.status = TaskTokenUsageStatus::Unavailable {
            unavailable_count: 3,
            reasons: vec![TaskTokenUsageUnavailableReason::AgentStarting],
        };
        rollup.backends[0].total = rollup.total.clone();
        for entry in &mut rollup.breakdown {
            entry.usage = TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::AgentStarting,
            };
        }
        dispatch_task_usage(&state, "h-task-unavail", 2, &rollup);
        next_tick().await;
        let popover = query(&container, ".session-task-popover").expect("popover stays open");
        assert!(
            popover
                .text_content()
                .unwrap_or_default()
                .contains("agent starting"),
            "pending native startup must show its server-owned unavailable reason"
        );
        let badge = query(&container, ".session-task-toggle").expect("unavailable badge stays");
        let badge_text = badge.text_content().unwrap_or_default();
        assert!(
            badge_text.contains("unavailable")
                && !badge_text.contains('Σ')
                && !badge_text.contains('0'),
            "pending startup must not fabricate a real zero"
        );
    }

    /// The task usage display must not depend on the session-settings surface:
    /// a backend with an empty settings schema (Tycode-style) renders nothing
    /// while there is no rollup, then a display-only footer row once the
    /// server reports a task — never a settings toggle.
    #[wasm_bindgen_test]
    async fn task_badge_renders_without_settings_schema() {
        let container = make_container();
        let state = make_state("h-task-noschema", "root", false);
        let _handle = mount_bar(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-settings-accordion").is_none(),
            "no footer row may render with neither settings nor task usage"
        );

        dispatch_task_usage(
            &state,
            "h-task-noschema",
            0,
            &partial_rollup("root", 500, 100),
        );
        for _ in 0..4 {
            next_tick().await;
        }
        let badge = query(&container, ".session-task-toggle")
            .expect("task display should render without any settings schema");
        assert!(
            badge
                .text_content()
                .unwrap_or_default()
                .contains("including 3 agents"),
            "display should render the rollup agent count"
        );
        assert!(
            query(&container, ".session-settings-toggle").is_none(),
            "an empty settings schema must not grow a settings toggle"
        );

        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_some(),
            "breakdown must open from the display-only row too"
        );
    }

    /// When a sub-agent's tab is active, the footer still shows the whole
    /// task's total: rollups are keyed by the root agent, so the display
    /// resolves the same-host rollup whose breakdown contains the active
    /// sub-agent, rather than showing nothing.
    #[wasm_bindgen_test]
    async fn sub_agent_tab_resolves_parent_rollup() {
        let container = make_container();
        // Active tab is the sub-agent "child-a"; the task rollup is keyed by
        // the root "root" and lists child-a among its breakdown entries.
        let state = make_state_with_active_agent("h-task-sub", "child-a");
        dispatch_task_usage(&state, "h-task-sub", 0, &partial_rollup("root", 500, 100));
        let _handle = mount_bar(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let badge = query(&container, ".session-task-toggle")
            .expect("sub-agent tab must resolve the parent task rollup");
        let badge_text = badge.text_content().unwrap_or_default();
        assert!(
            badge_text.contains("\u{2191}1.5K") && badge_text.contains("\u{2193}300"),
            "sub-agent footer must show the whole-task total, got: {badge_text}"
        );
        assert!(
            badge_text.contains("including 3 agents"),
            "sub-agent footer must still count all task agents, got: {badge_text}"
        );

        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }
        expand_usage(&container, "Agents").await;
        let popover = query(&container, ".session-task-popover")
            .expect("breakdown opens from a sub-agent tab too");
        let popover_text = popover.text_content().unwrap_or_default();
        assert!(
            popover_text.contains("Root")
                && popover_text.contains("Child A")
                && popover_text.contains("Child B"),
            "breakdown must list the full task tree, got: {popover_text}"
        );
    }

    /// Regression: live agent churn during a multi-agent task (activity
    /// summaries on the active agent, unrelated spawns) notifies the raw
    /// `state.agents` vec but must not recreate the footer subtree — that
    /// would silently reset a pinned popover. The outer bar depends only on
    /// deduped semantic memos, so the popover stays open and functional.
    #[wasm_bindgen_test]
    async fn task_popover_survives_agent_list_churn() {
        let container = make_container();
        let state = make_state_with_active_agent("h-task-churn", "root");
        dispatch_task_usage(&state, "h-task-churn", 0, &partial_rollup("root", 500, 100));
        let _handle = mount_bar(&container, state.clone());
        for _ in 0..4 {
            next_tick().await;
        }

        let badge = query(&container, ".session-task-toggle").expect("task badge renders");
        badge.click();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(query(&container, ".session-task-popover").is_some());

        // Activity-summary churn on the active agent: the agents vec is
        // notified, but the agent's (host, backend) binding is unchanged.
        let summary = Envelope::from_payload(
            StreamPath("/host/h-task-churn".to_owned()),
            FrameKind::AgentActivitySummary,
            1,
            &AgentActivitySummaryPayload {
                agent_id: AgentId("root".to_owned()),
                state: AgentActivitySummaryState::Empty,
            },
        )
        .expect("envelope serialize");
        crate::dispatch::dispatch_envelope(&state, "h-task-churn", summary);
        // An unrelated agent spawning on the same host notifies the vec too.
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                tyde_permissions: Default::default(),
                host_id: "h-task-churn".to_owned(),
                agent_id: AgentId("bystander".to_owned()),
                name: "Bystander".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                team_member_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/bystander/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
                review_recipient: false,
            });
        });
        for _ in 0..4 {
            next_tick().await;
        }

        let popover = query(&container, ".session-task-popover")
            .expect("agent-list churn must not close the pinned popover");
        assert!(
            popover
                .text_content()
                .unwrap_or_default()
                .contains("3 agents"),
            "popover content must remain intact across the churn"
        );
        let badge = query(&container, ".session-task-toggle").expect("badge still rendered");
        assert_eq!(
            badge.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );

        // The badge still toggles normally after the churn.
        badge.click();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_none(),
            "the badge must still close the popover after agent churn"
        );
    }

    /// Mirrors what `claude.rs` `map_passive_rate_limit_event` actually emits:
    /// one representative bucket, no scope, no window, always a vendor status,
    /// no plan label, and `vendor_reported: true` — the vendor supplied the used
    /// magnitude, while the remaining percentage is Tyde's derived complement.
    fn dispatch_capacity(state: &AppState, host_id: &str, seq: u64, used_percent: u8) {
        let snapshot = protocol::BackendCapacitySnapshot {
            backend_kind: BackendKind::Claude,
            state: protocol::BackendCapacityState::Known {
                report: protocol::CapacityReport {
                    source: protocol::CapacitySource::ClaudeRateLimitEvent,
                    observed_at_ms: None,
                    plan: None,
                    buckets: vec![protocol::CapacityBucket {
                        id: protocol::CapacityBucketId::Claude {
                            limit: protocol::ClaudeLimitType::SevenDay,
                        },
                        label: "weekly limit".to_owned(),
                        measure: protocol::CapacityMeasure::UsedPercent {
                            used_percent,
                            remaining_percent: 100 - used_percent,
                            provenance: protocol::ValueProvenance {
                                vendor_reported: true,
                            },
                        },
                        scope: protocol::CapacityScope::NotReported,
                        window: protocol::CapacityWindow::NotReported,
                        reset: protocol::CapacityReset::NotReported,
                        status: Some(protocol::CapacityBucketStatus::AllowedWarning),
                    }],
                    coverage: protocol::CapacityCoverage::RepresentativeBucketOnly,
                },
            },
            retrieved_at_ms: 0,
            freshness: protocol::CapacityFreshness::Fresh { age_ms: 0 },
            refreshable: true,
        };
        let envelope = Envelope::from_payload(
            StreamPath(format!("/host/{host_id}")),
            FrameKind::BackendCapacity,
            seq,
            &protocol::BackendCapacityPayload {
                snapshots: vec![snapshot],
            },
        )
        .expect("envelope serialize");
        crate::dispatch::dispatch_envelope(state, host_id, envelope);
    }

    /// Subscription capacity and task token usage are different measurements of
    /// different things — this task's tokens versus the account's quota — and
    /// they are not summable. They must therefore live in separate labelled
    /// regions inside the popup, with neither region's text containing the
    /// other's figures. A layout that invites the arithmetic is the bug.
    #[wasm_bindgen_test]
    async fn capacity_row_is_separate_from_task_token_usage() {
        let container = make_container();
        let state = make_state_with_active_agent("h-cap-popup", "root");
        dispatch_task_usage(&state, "h-cap-popup", 0, &partial_rollup("root", 500, 100));
        dispatch_capacity(&state, "h-cap-popup", 1, 82);
        let _handle = mount_bar(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }

        // Capacity lives in the popup, not on the always-visible badge — the
        // badge is the task figure and must not grow a quota number.
        let badge = query(&container, ".session-task-toggle").expect("task badge renders");
        let badge_text = badge.text_content().unwrap_or_default();
        assert!(
            !badge_text.contains("82%") && !badge_text.contains("Subscription"),
            "the task badge must not carry subscription capacity, got: {badge_text}"
        );

        badge.click();
        for _ in 0..4 {
            next_tick().await;
        }

        let capacity = query(&container, ".task-capacity-limits")
            .expect("the popup carries the compact capacity row");
        // The compact group now names all four components; exact-matching
        // the former shorter label misses the existing rendered region.
        let task_rows = query(
            &container,
            "[aria-label=\"Claude task token usage: input, cached input, output and reasoning\"]",
        )
        .expect("the popup carries the explicitly labelled four-component task breakdown");

        // Two distinct labelled regions, neither nested in the other.
        assert_eq!(capacity.get_attribute("role").as_deref(), Some("group"));
        assert_eq!(task_rows.get_attribute("role").as_deref(), Some("group"));
        let capacity_label = capacity.get_attribute("aria-label").unwrap_or_default();
        let task_label = task_rows.get_attribute("aria-label").unwrap_or_default();
        assert!(
            capacity_label.contains("account limits"),
            "capacity region must name itself, got: {capacity_label}"
        );
        assert!(
            task_label.contains("task token usage"),
            "task region must name itself, got: {task_label}"
        );
        assert!(
            !task_rows.contains(Some(&capacity)) && !capacity.contains(Some(&task_rows)),
            "capacity and task usage must not nest inside one another"
        );

        // Neither region's text carries the other's figures.
        let capacity_text = capacity.text_content().unwrap_or_default();
        let task_text = task_rows.text_content().unwrap_or_default();
        assert!(
            capacity_text.contains("82%") && capacity_text.contains("weekly limit"),
            "capacity region shows the quota figure, got: {capacity_text}"
        );
        assert!(
            capacity_label.contains("Claude") && capacity_label.contains("account limits"),
            "capacity region names the reporting vendor, got: {capacity_text}"
        );
        assert!(
            !capacity_text.contains('\u{2191}') && !capacity_text.contains('\u{2193}'),
            "capacity region must not carry task token figures, got: {capacity_text}"
        );
        assert!(
            !task_text.contains("82%") && !task_text.contains("Subscription"),
            "task region must not carry subscription capacity, got: {task_text}"
        );

        // The rendered concise notice says "Other limits unknown"; requiring
        // the old verbose copy rejects the requested UI, not missing coverage.
        let coverage_notice = query(&capacity, ".task-limit-context [role=note]").unwrap();
        assert_eq!(coverage_notice.inner_text(), "Other limits unknown");
        assert!(coverage_notice.get_bounding_client_rect().height() > 0.0);
        assert!(
            coverage_notice
                .get_attribute("aria-label")
                .unwrap()
                .contains("only the limit that is currently binding"),
            "representative coverage retains the binding-only provenance for assistive technology"
        );
        coverage_notice.focus().unwrap();
        next_tick().await;
        let tooltip = query(&coverage_notice, "[role=tooltip]").unwrap();
        let tooltip_rect = tooltip.get_bounding_client_rect();
        let popup_rect = query(&container, ".session-task-popover")
            .unwrap()
            .get_bounding_client_rect();
        assert!(
            tooltip_rect.height() > 0.0
                && tooltip_rect.left() >= popup_rect.left()
                && tooltip_rect.right() <= popup_rect.right()
                && tooltip_rect.top() >= popup_rect.top()
                && tooltip_rect.bottom() <= popup_rect.bottom(),
            "the complete keyboard explanation stays inside the visible popup, including the rightmost quota column"
        );
        let document = web_sys::window().unwrap().document().unwrap();
        let hit = document
            .element_from_point(
                (tooltip_rect.left() + tooltip_rect.width() / 2.0) as f32,
                (tooltip_rect.top() + tooltip_rect.height() / 2.0) as f32,
            )
            .unwrap();
        assert!(
            tooltip.contains(Some(&hit)),
            "the explanation is actually painted and not clipped by the scrolling body"
        );
        tooltip.focus().unwrap();
        next_tick().await;
        assert!(
            tooltip.get_bounding_client_rect().height() > 0.0,
            "long explanations remain available when focus enters their scrollable text"
        );

        // Passive sources: nothing to refresh, so no refresh control.
        assert!(
            query(&container, ".task-capacity-limits button").is_none(),
            "phase 1 capacity must not offer a refresh button"
        );
    }

    /// Dismissal paths: pointer-down outside closes the popover, Escape
    /// closes it without the toggle being focused, click/tap toggles it, and
    /// neither mouse nor touch hover opens it. `aria-expanded` tracks the
    /// actual visible state throughout.
    #[wasm_bindgen_test]
    async fn task_popover_dismissal_paths() {
        let container = make_container();
        let state = make_state_with_active_agent("h-task-dismiss", "root");
        dispatch_task_usage(
            &state,
            "h-task-dismiss",
            0,
            &partial_rollup("root", 500, 100),
        );
        let _handle = mount_bar(&container, state);
        for _ in 0..4 {
            next_tick().await;
        }
        let document = web_sys::window().unwrap().document().unwrap();
        let badge = query(&container, ".session-task-toggle").expect("task badge renders");
        assert_eq!(
            badge.get_attribute("aria-haspopup").as_deref(),
            Some("dialog")
        );

        // Outside pointer-down closes a click-opened popover.
        badge.click();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(query(&container, ".session-task-popover").is_some());
        document
            .body()
            .unwrap()
            .dispatch_event(&pointer_event("pointerdown", "mouse", true))
            .unwrap();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_none(),
            "pointer-down outside the badge must close the popover"
        );

        // A pointer-down inside the popover must NOT close it.
        badge.click();
        for _ in 0..2 {
            next_tick().await;
        }
        let popover = query(&container, ".session-task-popover").expect("popover reopens");
        popover
            .dispatch_event(&pointer_event("pointerdown", "mouse", true))
            .unwrap();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_some(),
            "pointer-down inside the popover must not dismiss it"
        );

        // Escape closes it even though focus is nowhere near the toggle.
        let escape_init = web_sys::KeyboardEventInit::new();
        escape_init.set_key("Escape");
        escape_init.set_bubbles(true);
        let escape =
            web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &escape_init)
                .expect("keyboard event");
        document.body().unwrap().dispatch_event(&escape).unwrap();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_none(),
            "Escape must close the popover regardless of focus"
        );

        // Hover is inert for both touch and mouse; only click/tap opens it.
        let wrapper = query(&container, ".session-task-usage").expect("badge wrapper");
        for pointer_type in ["touch", "mouse"] {
            wrapper
                .dispatch_event(&pointer_event("pointerenter", pointer_type, false))
                .unwrap();
            wrapper
                .dispatch_event(&pointer_event("pointerleave", pointer_type, false))
                .unwrap();
            for _ in 0..2 {
                next_tick().await;
            }
            assert!(
                query(&container, ".session-task-popover").is_none(),
                "{pointer_type} hover must not open the click-only popover"
            );
        }
        badge.click();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(query(&container, ".session-task-popover").is_some());
        assert_eq!(
            badge.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );
        badge.click();
        for _ in 0..2 {
            next_tick().await;
        }
        assert!(
            query(&container, ".session-task-popover").is_none(),
            "a second click must close the popover"
        );
        assert_eq!(
            badge.get_attribute("aria-expanded").as_deref(),
            Some("false")
        );
    }
}
