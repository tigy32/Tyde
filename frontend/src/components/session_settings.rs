use leptos::prelude::*;
use wasm_bindgen::JsCast;

use protocol::{
    BackendKind, SelectOption, SessionSchemaEntry, SessionSettingFieldType, SessionSettingValue,
    SessionSettingsSchema, SessionSettingsValues, TaskTokenUsageAmount, TaskTokenUsagePayload,
    TaskTokenUsageScope, TaskTokenUsageStatus, TaskTokenUsageUnavailableReason,
};

use crate::components::agents_panel::backend_label;
use crate::components::backend_capacity::CapacityCompactRow;
use crate::components::chat_message::{format_compact, token_badge_data};
use crate::state::{ActiveAgentRef, AppState, ComposerHandle};

pub(crate) fn clear_invalid_dependent_select_values(
    fields: &[protocol::SessionSettingField],
    values: &mut SessionSettingsValues,
) {
    for field in fields {
        if field.select_options_by_setting.is_none() {
            continue;
        }
        let Some(SessionSettingValue::String(selected)) = values.0.get(&field.key) else {
            continue;
        };
        let selected = selected.clone();
        let valid = field
            .select_options(values)
            .is_some_and(|options| options.iter().any(|option| option.value == selected));
        if !valid
            && matches!(
                &field.field_type,
                SessionSettingFieldType::Select { nullable: true, .. }
            )
        {
            values
                .0
                .insert(field.key.clone(), SessionSettingValue::Null);
        }
    }
}

/// Suffix marking a value the session carries but the schema no longer offers.
pub(crate) const UNAVAILABLE_OPTION_SUFFIX: &str = " (unavailable)";

/// Marker a backend puts in an option's **label** for a choice it deliberately
/// retained but cannot currently serve — the Hermes profile schema emits
/// `"<name> — Unavailable: <error>"` for a profile whose gateway probe failed,
/// so the option stays visible with its reason instead of vanishing.
///
/// Matching on the label is not a preference. [`SelectOption`] carries only
/// `value` and `label`, so this is the sole channel the protocol offers; a
/// typed availability field on `SelectOption` is the durable fix and is tracked
/// as a protocol dependency. Retaining an option the backend has already said
/// it cannot serve, while leaving it selectable, would be worse than either.
pub(crate) const SCHEMA_UNAVAILABLE_MARKER: &str = "Unavailable:";

/// One entry of a select-like control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ControlOption {
    pub value: String,
    pub label: String,
    /// True for the synthetic entry standing in for a value the schema no
    /// longer offers. Display-only: the control must render it as selected but
    /// non-actionable, and no commit path may submit it as a user edit.
    pub unavailable: bool,
}

/// The entries a select-like control should render, given the value the session
/// actually carries.
///
/// A value the schema does not offer is still the truth about this session, so
/// it is appended as an explicitly-labelled entry instead of being dropped.
/// Dropping it makes the control display some *other* option — the schema
/// default, or whichever the browser picks first — and silently misreport the
/// session's real setting. That is the Hermes named-profile case: a profile
/// whose gateway probe failed leaves the schema, and the Profile control then
/// claims the session is on `default` when it is not.
///
/// The injected entry is display-only, and its `unavailable` flag is what the
/// commit paths check to keep it that way. Offering it as a selectable value
/// would invite the user to "set" a setting the backend has already said it
/// cannot serve.
pub(crate) fn options_including_current(
    options: &[SelectOption],
    current: &str,
) -> Vec<ControlOption> {
    let mut entries: Vec<ControlOption> = options
        .iter()
        .map(|option| ControlOption {
            value: option.value.clone(),
            // A schema option can itself be unavailable: the backend retains a
            // failed choice so its reason stays visible. Such an option must
            // read as non-actionable for exactly the same reason the synthetic
            // one below does.
            unavailable: option.label.contains(SCHEMA_UNAVAILABLE_MARKER),
            label: option.label.clone(),
        })
        .collect();
    if current.is_empty() || entries.iter().any(|entry| entry.value == current) {
        return entries;
    }
    entries.push(ControlOption {
        value: current.to_owned(),
        label: format!("{current}{UNAVAILABLE_OPTION_SUFFIX}"),
        unavailable: true,
    });
    entries
}

#[component]
pub fn SessionSettingsControls(
    schema: SessionSettingsSchema,
    values: Signal<SessionSettingsValues>,
    on_change: Callback<SessionSettingsValues>,
) -> impl IntoView {
    let fields = schema.fields.clone();
    let all_fields = schema.fields;

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
        </div>
    }
}

#[component]
pub fn SessionSettingsBar(
    /// Live-agent identity of the chat this bar belongs to; `None` for a draft.
    /// Explicit rather than derived from `active_agent` because every chat pane
    /// renders its own bar, and the unfocused pane must show — and edit — its
    /// own chat's settings.
    agent_ref: Signal<Option<ActiveAgentRef>>,
    /// The same chat's composer, holding the draft session settings shown
    /// before an agent exists.
    composer: ComposerHandle,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let expanded = RwSignal::new(false);

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
    let schema_status = {
        let state = state.clone();
        Memo::new(
            move |_| -> Option<(BackendKind, Option<SessionSchemaEntry>)> {
                let (host_id, backend_kind) = current_binding.get()?;
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
            BackendKind::Kiro => "Session Settings (Kiro)",
            BackendKind::Tycode => "Session Settings",
        }
    }

    fn missing_schema_message(backend_kind: BackendKind) -> &'static str {
        match backend_kind {
            BackendKind::Kiro => "Kiro models unavailable — check installation",
            BackendKind::Claude => "Claude settings unavailable — check installation",
            BackendKind::Codex => "Codex settings unavailable — check installation",
            BackendKind::Antigravity => "Antigravity settings unavailable — check installation",
            BackendKind::Hermes => "Hermes settings unavailable — check installation",
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

    view! {
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
                                <SessionSettingsControls
                                    schema=schema.clone()
                                    values=current_values
                                    on_change=on_change
                                />
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
    }
}

fn task_usage_unavailable_text(reason: TaskTokenUsageUnavailableReason) -> &'static str {
    match reason {
        TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted => "no completed turns",
        TaskTokenUsageUnavailableReason::BackendDidNotReport => "backend did not report",
        TaskTokenUsageUnavailableReason::ProviderScopeAmbiguous => "provider scope ambiguous",
        TaskTokenUsageUnavailableReason::AgentUnavailable => "agent unavailable",
    }
}

/// `(input_text, output_text)` for a rollup amount, formatted by the same
/// `token_badge_data` the per-agent badges use. When the server only knows the
/// combined figure (per-scope splits summed away by an unavailable entry) the
/// pair collapses to a single `Σtotal` text with no output slot.
fn task_amount_texts(amount: &TaskTokenUsageAmount) -> (String, Option<String>) {
    match (amount.input_tokens, amount.output_tokens) {
        (Some(input_tokens), Some(output_tokens)) => {
            let usage = protocol::TokenUsage {
                input_tokens,
                output_tokens,
                total_tokens: amount.total_tokens,
                cached_prompt_tokens: amount.cached_prompt_tokens,
                cache_creation_input_tokens: amount.cache_creation_input_tokens,
                reasoning_tokens: amount.reasoning_tokens,
            };
            let (input_text, output_text, _) = token_badge_data(&usage);
            (input_text, Some(output_text))
        }
        _ => (
            format!("\u{03a3}{}", format_compact(amount.total_tokens)),
            None,
        ),
    }
}

fn task_scope_view(scope: &TaskTokenUsageScope) -> AnyView {
    match scope {
        TaskTokenUsageScope::Known { usage } => {
            let (input_text, output_text) = task_amount_texts(usage);
            view! {
                <span class="session-task-row-usage">
                    <span class="token-stat token-stat-input">{input_text}</span>
                    {output_text.map(|text| view! {
                        <span class="token-stat token-stat-output">{text}</span>
                    })}
                </span>
            }
            .into_any()
        }
        TaskTokenUsageScope::Partial {
            usage,
            unavailable_count,
            ..
        } => {
            let (input_text, output_text) = task_amount_texts(usage);
            let marker = format!("partial \u{b7} {unavailable_count} unreported");
            view! {
                <span class="session-task-row-usage">
                    <span class="token-stat token-stat-input">{input_text}</span>
                    {output_text.map(|text| view! {
                        <span class="token-stat token-stat-output">{text}</span>
                    })}
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

/// The single task-wide token usage display for the active agent's whole task
/// (root + sub-agents), rendered once at the right edge of the session-settings
/// footer. It shows a left-style `↑input ↓output` string sourced from the
/// server's authoritative `TaskTokenUsagePayload.total` — never summed from
/// chat rows — and appends `including N agents` for multi-agent tasks (N is the
/// breakdown length). Partial totals carry a subtle `partial` marker; an
/// all-unavailable total shows a muted marker rather than a fabricated zero.
/// Click/Enter toggles the per-agent breakdown popover. Escape and pointer-down
/// outside dismiss it; hovering alone never opens it.
///
/// The popover also carries the compact subscription-capacity row for the
/// current agent's backend, in a *separate* labelled region below the task
/// breakdown. Task tokens are this task; subscription capacity is the account's
/// quota. They are not summable, so nothing in the layout or the accessible
/// text combines them, and capacity is never derived from these token figures.
#[component]
fn TaskUsageBadge(
    rollup: Memo<Option<TaskTokenUsagePayload>>,
    binding: Memo<Option<(String, BackendKind)>>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let wrapper_ref: NodeRef<leptos::html::Span> = NodeRef::new();

    // Dismissal listeners live on the window: a pointer-down anywhere outside
    // the badge/popover region closes it, as does Escape regardless of focus.
    // They use try_* accessors because window callbacks can race owner
    // disposal, and are explicitly removed on cleanup (window listeners are
    // not tied to the ownership tree).
    let outside_pointer = window_event_listener(leptos::ev::pointerdown, move |ev| {
        if !open.try_get_untracked().unwrap_or(false) {
            return;
        }
        let inside = wrapper_ref
            .try_get_untracked()
            .flatten()
            .is_some_and(|wrapper| {
                ev.target()
                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    .is_some_and(|node| wrapper.contains(Some(&node)))
            });
        if !inside {
            open.try_set(false);
        }
    });
    let escape_key = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && open.try_get_untracked().unwrap_or(false) {
            open.try_set(false);
        }
    });
    on_cleanup(move || {
        outside_pointer.remove();
        escape_key.remove();
    });

    view! {
        {move || {
            let payload = rollup.get()?;
            let agent_total = payload.breakdown.len();
            let multi_agent = agent_total > 1;
            // The figure is the server's authoritative combined task total,
            // formatted in the same left-style `↑input ↓output` string the
            // per-message badges use. An all-unavailable aggregate carries no
            // real figure — show a muted marker, never a fabricated zero.
            let usage_texts = match &payload.total.status {
                TaskTokenUsageStatus::Unavailable { .. } => None,
                TaskTokenUsageStatus::Known | TaskTokenUsageStatus::Partial { .. } => {
                    Some(task_amount_texts(&payload.total.usage))
                }
            };
            let partial = matches!(payload.total.status, TaskTokenUsageStatus::Partial { .. });
            let including_text = multi_agent.then(|| format!("including {agent_total} agents"));
            let status_note = match &payload.total.status {
                TaskTokenUsageStatus::Known => None,
                TaskTokenUsageStatus::Partial {
                    unavailable_count, ..
                } => Some(format!("partial \u{b7} {unavailable_count} unreported")),
                TaskTokenUsageStatus::Unavailable { .. } => Some("unavailable".to_owned()),
            };
            let popover_id = format!("session-task-popover-{}", payload.root_agent_id);
            let popover_id_attr = popover_id.clone();
            let breakdown = payload.breakdown.clone();
            Some(view! {
                <span
                    class="session-task-usage"
                    node_ref=wrapper_ref
                >
                    <button
                        class="session-task-toggle"
                        aria-haspopup="dialog"
                        aria-expanded=move || open.get().to_string()
                        aria-controls=popover_id_attr
                        title="Cumulative tokens across this task's requests and \
                               agents. The context panel shows the latest \
                               request's occupancy instead."
                        on:click=move |_| open.update(|o| *o = !*o)
                    >
                        // Scope marker. This figure is task-wide and
                        // cumulative; the context bar is single-request
                        // occupancy. Unlabelled, the pair reads as bad
                        // arithmetic rather than two different measurements.
                        <span class="session-task-scope">"Task"</span>
                        {match usage_texts {
                            Some((input_text, output_text)) => view! {
                                <>
                                    <span class="token-stat token-stat-input">{input_text}</span>
                                    {output_text.map(|text| view! {
                                        <span class="token-stat token-stat-output">{text}</span>
                                    })}
                                </>
                            }.into_any(),
                            None => view! {
                                <span class="session-task-usage-unavailable">
                                    "usage unavailable"
                                </span>
                            }.into_any(),
                        }}
                        {including_text.map(|text| view! {
                            <span class="session-task-including">{text}</span>
                        })}
                        {partial.then(|| view! {
                            <span class="session-task-status session-task-status-partial">
                                "partial"
                            </span>
                        })}
                    </button>
                    <Show when=move || open.get()>
                        <div
                            class="session-task-popover"
                            id=popover_id.clone()
                            role="dialog"
                            aria-label="Task token usage breakdown"
                        >
                            <div class="session-task-popover-title">
                                <span>{format!(
                                    "Task usage \u{b7} {agent_total} agent{}",
                                    if agent_total == 1 { "" } else { "s" },
                                )}</span>
                                {status_note.clone().map(|note| view! {
                                    <span class="session-task-popover-status">{note}</span>
                                })}
                            </div>
                            <div
                                class="session-task-popover-rows"
                                role="group"
                                aria-label="Task token usage by agent"
                            >
                                {breakdown.iter().map(|entry| {
                                    let indent = 6 + entry.depth.min(12) * 14;
                                    let meta = match &entry.model {
                                        Some(model) => {
                                            format!("{} \u{b7} {model}", backend_label(entry.backend_kind))
                                        }
                                        None => backend_label(entry.backend_kind).to_owned(),
                                    };
                                    view! {
                                        <div
                                            class="session-task-row"
                                            style=format!("padding-left: {indent}px")
                                        >
                                            <span class="session-task-row-name">
                                                {entry.name.clone()}
                                            </span>
                                            <span class="session-task-row-meta">{meta}</span>
                                            {task_scope_view(&entry.usage)}
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                            <CapacityCompactRow binding=binding />
                        </div>
                    </Show>
                </span>
            })
        }}
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AgentInfo, TabContent};
    use leptos::mount::mount_to;
    use protocol::{
        AgentActivitySummaryPayload, AgentActivitySummaryState, AgentId, AgentOrigin, Envelope,
        FrameKind, SelectOption, SelectOptionsBySetting, SelectOptionsForValue,
        SessionSettingField, StreamPath, TaskTokenUsageAggregate, TaskTokenUsageEntry,
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
                host_id: host_id.to_owned(),
                agent_id: AgentId(agent_id.to_owned()),
                name: "Root".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                instance_stream: StreamPath(format!("/agent/{agent_id}/inst")),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
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
        state.session_schemas.update(|map| {
            map.entry(host_id.to_owned()).or_default().insert(
                BackendKind::Claude,
                SessionSchemaEntry::Ready {
                    schema: SessionSettingsSchema {
                        backend_kind: BackendKind::Claude,
                        fields,
                    },
                },
            );
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
            view! { <SessionSettingsBar agent_ref=agent_ref composer=composer /> }
        })
    }

    fn known_amount(input: u64, output: u64) -> TaskTokenUsageAmount {
        TaskTokenUsageAmount {
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
            popover_text.contains("3 agents"),
            "popover title should count all breakdown rows, got: {popover_text}"
        );

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
        assert!(
            name_x(1) > name_x(0) && name_x(2) > name_x(0),
            "depth-1 rows must be indented past the root row"
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
        for entry in &mut rollup.breakdown {
            entry.usage = TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::NoAssistantTurnCompleted,
            };
        }
        dispatch_task_usage(&state, "h-task-unavail", 0, &rollup);
        let _handle = mount_bar(&container, state);
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
                host_id: "h-task-churn".to_owned(),
                agent_id: AgentId("bystander".to_owned()),
                name: "Bystander".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 1,
                instance_stream: StreamPath("/agent/bystander/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
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

        let capacity = query(&container, ".capacity-compact")
            .expect("the popup carries the compact capacity row");
        let task_rows = query(&container, ".session-task-popover-rows")
            .expect("the popup carries the task breakdown");

        // Two distinct labelled regions, neither nested in the other.
        assert_eq!(capacity.get_attribute("role").as_deref(), Some("group"));
        assert_eq!(task_rows.get_attribute("role").as_deref(), Some("group"));
        let capacity_label = capacity.get_attribute("aria-label").unwrap_or_default();
        let task_label = task_rows.get_attribute("aria-label").unwrap_or_default();
        assert!(
            capacity_label.contains("Subscription capacity"),
            "capacity region must name itself, got: {capacity_label}"
        );
        assert!(
            task_label.contains("Task token usage"),
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
            capacity_text.contains("82% used") && capacity_text.contains("weekly limit"),
            "capacity region shows the quota figure, got: {capacity_text}"
        );
        assert!(
            capacity_text.contains("Subscription") && capacity_text.contains("Claude"),
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

        // The coverage caveat is text in the popup, not a hover-only tooltip:
        // Claude reports only the binding limit, and the user must be told.
        assert!(
            capacity_text.contains("only the binding limit is reported"),
            "RepresentativeBucketOnly coverage must be stated in the popup, got: {capacity_text}"
        );

        // Passive sources: nothing to refresh, so no refresh control.
        assert!(
            query(&container, ".capacity-compact button").is_none(),
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
