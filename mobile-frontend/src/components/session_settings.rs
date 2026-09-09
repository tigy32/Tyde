use leptos::html::{Div, Select};
use leptos::prelude::*;
use protocol::{
    BackendKind, ControlOption, SessionSchemaEntry, SessionSettingFieldType, SessionSettingValue,
    SessionSettingsSchema, SessionSettingsValues, clear_invalid_dependent_select_values,
    options_including_current,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::components::chat_input::backend_label;
use crate::state::{ActiveAgentRef, AppState, LocalHostId};

/// Which settings the sheet is editing.
///
/// A draft is not a degenerate agent: its edits go to `draft_session_settings`
/// and are carried by the next spawn, while an agent's edits are a request the
/// server validates and echoes back. Keeping them one enum means the sheet
/// renders identically for both and only the commit differs.
#[derive(Clone, Debug, PartialEq)]
enum SettingsTarget {
    Agent(ActiveAgentRef),
    Draft,
}

fn sheet_title(backend_kind: BackendKind) -> String {
    format!("Session Settings ({})", backend_label(backend_kind))
}

/// The mobile session-settings sheet.
///
/// Mounted once, above the tab dock, and opened from the chat header's overflow
/// menu (for a running agent) or the new-chat options row (for a draft). These
/// settings are changed rarely, so they cost nothing hidden here and would cost
/// permanent chat real estate if shown.
#[component]
pub fn SessionSettingsSheet() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let open = state.session_settings_open;

    // What the sheet is bound to: the host whose schema applies, the backend
    // that defines the fields, and whether edits land on an agent or the draft.
    let binding_state = state.clone();
    let binding = Memo::new(
        move |_| -> Option<(LocalHostId, BackendKind, SettingsTarget)> {
            if let Some(active) = binding_state.active_agent.get() {
                let backend_kind = binding_state.agents.with(|agents| {
                    agents
                        .iter()
                        .find(|agent| {
                            agent.local_host_id == active.local_host_id
                                && agent.agent_id == active.agent_id
                        })
                        .map(|agent| agent.backend_kind)
                })?;
                return Some((
                    active.local_host_id.clone(),
                    backend_kind,
                    SettingsTarget::Agent(active),
                ));
            }
            let host = binding_state.active_local_host_id.get()?;
            let host_settings = binding_state.active_host_settings();
            let backend_kind = binding_state
                .draft_backend_override
                .get()
                .or_else(|| host_settings.as_ref().and_then(|s| s.default_backend))
                .or_else(|| {
                    host_settings
                        .as_ref()
                        .and_then(|s| s.enabled_backends.first().copied())
                })?;
            Some((host, backend_kind, SettingsTarget::Draft))
        },
    );

    // `None` while the host's schema snapshot has not arrived at all — which is
    // a different thing from a snapshot that arrived without this backend, and
    // must not be reported as "unavailable" during a reconnect.
    let schema_state = state.clone();
    let schema_entry = Memo::new(move |_| -> Option<Option<SessionSchemaEntry>> {
        let (host, backend_kind, _) = binding.get()?;
        schema_state.session_schemas_by_host.with(|by_host| {
            by_host
                .get(&host)
                .map(|schemas| schemas.get(&backend_kind).cloned())
        })
    });

    let values_state = state.clone();
    let values = Signal::derive(move || match binding.get().map(|(_, _, target)| target) {
        Some(SettingsTarget::Agent(active)) => values_state
            .agent_session_settings
            .with(|map| map.get(&active.as_agent_ref()).cloned())
            .unwrap_or_default(),
        Some(SettingsTarget::Draft) | None => values_state.draft_session_settings.get(),
    });

    // Bumped for every edit the server never received. A control that has just
    // been touched holds the user's own pick in the DOM, and nothing in the
    // reactive values changes when a send fails — so without a signal to track,
    // a rejected edit would sit on screen looking exactly like an applied one.
    let rejected_edits = RwSignal::new(0_u32);

    let change_state = state.clone();
    let on_change = Callback::new(move |new_values: SessionSettingsValues| {
        match binding.get_untracked().map(|(_, _, target)| target) {
            Some(SettingsTarget::Agent(active)) => {
                let state = change_state.clone();
                spawn_local(async move {
                    let agent_ref = active.as_agent_ref();
                    if let Err(error) =
                        crate::actions::set_session_settings(&state, &agent_ref, new_values).await
                    {
                        let message = format!(
                            "Setting change was not sent — this chat is still on its \
                             previous settings: {error}"
                        );
                        log::error!("{message}");
                        state
                            .mobile_shell_error
                            .set(Some(crate::state::MobileShellError {
                                code: protocol::MobileAccessErrorCode::TransportFailed,
                                message,
                            }));
                        rejected_edits.update(|count| *count += 1);
                    }
                });
            }
            Some(SettingsTarget::Draft) | None => {
                change_state.draft_session_settings.set(new_values);
            }
        }
    });

    let close_state = state.clone();
    let on_close = Callback::new(move |_: ()| close_state.session_settings_open.set(false));

    // Escape-to-cancel on the focused backdrop, matching `ProjectPicker`: the
    // listener is scoped to the sheet and torn down with it.
    let backdrop_ref: NodeRef<Div> = NodeRef::new();
    Effect::new(move |_| {
        if open.get()
            && let Some(element) = backdrop_ref.get()
        {
            let _ = element.focus();
        }
    });

    view! {
        <Show when=move || open.get()>
            <div
                node_ref=backdrop_ref
                class="session-settings-backdrop"
                role="dialog"
                aria-modal="true"
                aria-label="Session settings"
                tabindex="-1"
                data-mobile-test="session-settings-sheet"
                on:click=move |_| on_close.run(())
                on:keydown=move |event: web_sys::KeyboardEvent| {
                    if event.key() == "Escape" {
                        on_close.run(());
                    }
                }
            >
                <div
                    class="session-settings-sheet"
                    on:click=|event: web_sys::MouseEvent| event.stop_propagation()
                >
                    <div class="session-settings-sheet-header">
                        <h2 class="session-settings-sheet-title" data-mobile-test="session-settings-title">
                            {move || binding
                                .get()
                                .map(|(_, backend_kind, _)| sheet_title(backend_kind))
                                .unwrap_or_else(|| "Session Settings".to_owned())}
                        </h2>
                        <button
                            type="button"
                            class="session-settings-done"
                            data-mobile-test="session-settings-done"
                            on:click=move |_| on_close.run(())
                        >
                            "Done"
                        </button>
                    </div>
                    <p class="session-settings-scope" data-mobile-test="session-settings-scope">
                        {move || match binding.get().map(|(_, _, target)| target) {
                            Some(SettingsTarget::Agent(_)) => "Applies to this chat from the next message.",
                            Some(SettingsTarget::Draft) | None => "Applies to the chat you are about to start.",
                        }}
                    </p>
                    <div class="session-settings-sheet-body">
                        {move || {
                            let Some((_, backend_kind, _)) = binding.get() else {
                                return status_text(
                                    "No backend is selected for this chat yet.".to_owned(),
                                );
                            };
                            match schema_entry.get() {
                                None => status_text(format!(
                                    "{} settings are loading\u{2026}",
                                    backend_label(backend_kind)
                                )),
                                Some(Some(SessionSchemaEntry::Ready { schema }))
                                    if !schema.fields.is_empty() =>
                                {
                                    view! {
                                        <SessionSettingsControls
                                            schema=schema
                                            values=values
                                            on_change=on_change
                                            resync=rejected_edits.into()
                                        />
                                    }
                                    .into_any()
                                }
                                Some(Some(SessionSchemaEntry::Ready { .. })) => status_text(format!(
                                    "{} has no session settings to change.",
                                    backend_label(backend_kind)
                                )),
                                Some(Some(SessionSchemaEntry::Pending { .. })) => status_text(format!(
                                    "{} settings are loading\u{2026}",
                                    backend_label(backend_kind)
                                )),
                                Some(Some(SessionSchemaEntry::Unavailable { message, .. })) => {
                                    let text = if message.trim().is_empty() {
                                        format!(
                                            "{} settings are unavailable \u{2014} check the \
                                             backend on the host.",
                                            backend_label(backend_kind)
                                        )
                                    } else {
                                        message
                                    };
                                    status_text(text)
                                }
                                Some(None) => status_text(format!(
                                    "{} settings are unavailable \u{2014} check the backend on \
                                     the host.",
                                    backend_label(backend_kind)
                                )),
                            }
                        }}
                    </div>
                </div>
            </div>
        </Show>
    }
}

fn status_text(text: String) -> AnyView {
    view! {
        <p class="session-settings-status" data-mobile-test="session-settings-status">{text}</p>
    }
    .into_any()
}

/// The controls for one backend's schema.
///
/// Every field is one labelled row. `use_slider` is deliberately ignored: it is
/// a hint for the desktop's ordered range control, and a 20px-tall range input
/// is a worse way to pick "medium" on a phone than the platform's own picker
/// wheel, which is what a `<select>` gets us for free.
#[component]
fn SessionSettingsControls(
    schema: SessionSettingsSchema,
    values: Signal<SessionSettingsValues>,
    on_change: Callback<SessionSettingsValues>,
    /// Changes whenever an edit failed to reach the server. Every control reads
    /// it so the settings the agent actually carries are re-applied over a pick
    /// the DOM is still holding.
    resync: Signal<u32>,
) -> impl IntoView {
    let all_fields = schema.fields.clone();

    view! {
        <div class="session-settings-fields">
            {schema.fields.into_iter().map(|field| {
                let key = field.key.clone();
                let test_id = format!("session-setting-{}", field.key);
                let label = field.label.clone();
                let description = field.description.clone();
                let field_type = field.field_type.clone();
                let field_for_options = field.clone();
                let field_for_unknown = field.clone();
                let available_options = Memo::new(move |_| {
                    field_for_options
                        .select_options(&values.get())
                        .unwrap_or_default()
                        .to_vec()
                });
                // `select_options` answers `None` when this field's options
                // depend on another setting whose value it does not recognize.
                // Rendering that as an empty list would read as "this backend
                // has no models" rather than "Tyde cannot tell yet".
                let options_unknown = Memo::new(move |_| {
                    field_for_unknown.select_options(&values.get()).is_none()
                });
                let all_fields = all_fields.clone();

                view! {
                    <div class="session-setting">
                        <label class="session-setting-label" for=test_id.clone()>{label}</label>
                        {match field_type {
                            SessionSettingFieldType::Select { default, nullable, .. } => {
                                let key = key.clone();
                                let current_value = {
                                    let key = key.clone();
                                    Signal::derive(move || {
                                        match values.get().0.get(&key) {
                                            Some(SessionSettingValue::String(value)) => value.clone(),
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
                                    move |event: leptos::ev::Event| {
                                        let selected = event_target_value(&event);
                                        // The synthetic entry renders disabled, so
                                        // a browser will not fire this for it — the
                                        // guard is what makes "display-only" a
                                        // property of the commit path rather than
                                        // of the markup.
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
                                            current
                                                .0
                                                .insert(key.clone(), SessionSettingValue::String(selected));
                                        }
                                        clear_invalid_dependent_select_values(&all_fields, &mut current);
                                        on_change.run(current);
                                    }
                                };

                                // A `<select>` ignores a value with no matching
                                // `<option>` yet, and the options here are
                                // reactive children applied after the element's
                                // properties. Re-applying the value from a node
                                // ref, tracking the option list, is what makes a
                                // late-arriving option take effect instead of the
                                // control silently showing the browser's pick.
                                let select_ref: NodeRef<Select> = NodeRef::new();
                                Effect::new(move |_| {
                                    let value = current_value.get();
                                    let _ = available_options.get();
                                    let _ = resync.get();
                                    if let Some(element) = select_ref.get() {
                                        element.set_value(&value);
                                    }
                                });

                                view! {
                                    <select
                                        id=test_id.clone()
                                        class="session-setting-select"
                                        data-mobile-test=test_id.clone()
                                        node_ref=select_ref
                                        prop:value=move || current_value.get()
                                        on:change=on_select_change
                                    >
                                        {nullable.then(|| view! { <option value="">"Auto"</option> })}
                                        // The session's own value is listed even
                                        // when the schema stopped offering it, so
                                        // the control cannot fall back to showing
                                        // a different option as if it were the
                                        // setting in effect.
                                        {move || options_including_current(
                                            &available_options.get(),
                                            &current_value.get(),
                                        )
                                            .into_iter()
                                            .map(|entry| {
                                                let ControlOption { value, label, unavailable } = entry;
                                                view! {
                                                    <option value=value disabled=unavailable>{label}</option>
                                                }
                                            })
                                            .collect_view()}
                                        {move || (options_unknown.get()
                                            && available_options.get().is_empty())
                                            .then(|| view! {
                                                <option value="" disabled=true>
                                                    "Unavailable \u{2014} depends on another \
                                                     setting Tyde cannot resolve"
                                                </option>
                                            })}
                                    </select>
                                }
                                .into_any()
                            }
                            SessionSettingFieldType::Toggle { default } => {
                                let key = key.clone();
                                let current_checked = {
                                    let key = key.clone();
                                    move || {
                                        let _ = resync.get();
                                        match values.get().0.get(&key) {
                                            Some(SessionSettingValue::Bool(value)) => *value,
                                            _ => default,
                                        }
                                    }
                                };
                                let on_toggle_change = {
                                    let key = key.clone();
                                    move |event: leptos::ev::Event| {
                                        let input: web_sys::HtmlInputElement =
                                            event.target().unwrap().unchecked_into();
                                        let mut current = values.get_untracked();
                                        current
                                            .0
                                            .insert(key.clone(), SessionSettingValue::Bool(input.checked()));
                                        on_change.run(current);
                                    }
                                };

                                view! {
                                    <input
                                        id=test_id.clone()
                                        type="checkbox"
                                        class="session-setting-toggle"
                                        data-mobile-test=test_id.clone()
                                        prop:checked=current_checked
                                        on:change=on_toggle_change
                                    />
                                }
                                .into_any()
                            }
                            SessionSettingFieldType::Integer { min, max, step, default } => {
                                let key = key.clone();
                                let current_int = {
                                    let key = key.clone();
                                    move || {
                                        let _ = resync.get();
                                        match values.get().0.get(&key) {
                                            Some(SessionSettingValue::Integer(value)) => *value,
                                            _ => default,
                                        }
                                    }
                                };
                                let on_int_change = {
                                    let key = key.clone();
                                    move |event: leptos::ev::Event| {
                                        if let Ok(parsed) = event_target_value(&event).parse::<i64>() {
                                            let mut current = values.get_untracked();
                                            current.0.insert(
                                                key.clone(),
                                                SessionSettingValue::Integer(parsed.clamp(min, max)),
                                            );
                                            on_change.run(current);
                                        }
                                    }
                                };

                                view! {
                                    <input
                                        id=test_id.clone()
                                        type="number"
                                        class="session-setting-number"
                                        data-mobile-test=test_id.clone()
                                        inputmode="numeric"
                                        min=min.to_string()
                                        max=max.to_string()
                                        step=step.to_string()
                                        autocomplete="off"
                                        prop:value=move || current_int().to_string()
                                        on:change=on_int_change
                                    />
                                }
                                .into_any()
                            }
                        }}
                        // Shown, not put in a `title`: a tooltip never renders on
                        // a touch UI, so a description hidden in one is a
                        // description nobody on this surface can read.
                        {description.map(|text| view! {
                            <p class="session-setting-hint">{text}</p>
                        })}
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AgentInfo, AppState};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, SelectOption, SelectOptionsBySetting, SelectOptionsForValue,
        SessionSettingField, StreamPath,
    };
    use settings_model::HostSettings;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlSelectElement};

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

    fn select_option(value: &str, label: &str) -> SelectOption {
        SelectOption {
            value: value.to_owned(),
            label: label.to_owned(),
        }
    }

    /// A profile-keyed model list: the shape that makes a stale dependent value
    /// possible, and the reason the commit path clears one.
    fn schema() -> SessionSettingsSchema {
        SessionSettingsSchema {
            backend_kind: BackendKind::Claude,
            fields: vec![
                SessionSettingField {
                    key: "profile".to_owned(),
                    label: "Profile".to_owned(),
                    description: None,
                    field_type: SessionSettingFieldType::Select {
                        options: vec![select_option("fast", "Fast"), select_option("deep", "Deep")],
                        default: Some("fast".to_owned()),
                        nullable: false,
                    },
                    use_slider: false,
                    select_options_by_setting: None,
                },
                SessionSettingField {
                    key: "model".to_owned(),
                    label: "Model".to_owned(),
                    description: Some("Which model answers this chat".to_owned()),
                    field_type: SessionSettingFieldType::Select {
                        options: vec![select_option("haiku", "Haiku")],
                        default: None,
                        nullable: true,
                    },
                    use_slider: false,
                    select_options_by_setting: Some(SelectOptionsBySetting {
                        setting_key: "profile".to_owned(),
                        values: vec![
                            SelectOptionsForValue {
                                setting_value: "fast".to_owned(),
                                options: vec![select_option("haiku", "Haiku")],
                            },
                            SelectOptionsForValue {
                                setting_value: "deep".to_owned(),
                                options: vec![select_option("opus", "Opus")],
                            },
                        ],
                    }),
                },
            ],
        }
    }

    fn host_id() -> LocalHostId {
        LocalHostId("settings-host".to_owned())
    }

    fn seed_host(state: &AppState) {
        let host = host_id();
        state.active_local_host_id.set(Some(host.clone()));
        state.session_schemas_by_host.update(|by_host| {
            let mut schemas = std::collections::HashMap::new();
            schemas.insert(
                BackendKind::Claude,
                SessionSchemaEntry::Ready { schema: schema() },
            );
            by_host.insert(host, schemas);
        });
    }

    fn seed_agent(state: &AppState) -> ActiveAgentRef {
        let host = host_id();
        let agent_id = AgentId("settings-agent".to_owned());
        state.agents.set(vec![AgentInfo {
            local_host_id: host.clone(),
            agent_id: agent_id.clone(),
            name: "Agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath("/agent/settings/inst".to_owned()),
            started: true,
            fatal_error: None,
        }]);
        let active = ActiveAgentRef {
            local_host_id: host,
            agent_id,
        };
        state.active_agent.set(Some(active.clone()));
        active
    }

    fn mount_sheet(container: &HtmlElement, state: AppState) {
        let state_for_mount = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <SessionSettingsSheet /> }
        });
        std::mem::forget(handle);
    }

    fn control(container: &HtmlElement, key: &str) -> HtmlSelectElement {
        container
            .query_selector(&format!("[data-mobile-test='session-setting-{key}']"))
            .unwrap()
            .unwrap_or_else(|| panic!("the {key} control must render"))
            .dyn_into()
            .unwrap()
    }

    /// **A live agent's settings can be changed from the sheet, and the change
    /// actually reaches that agent's stream.**
    ///
    /// The whole surface exists to send this one frame; before it, mobile
    /// received schemas and settings and rendered neither, so a phone could not
    /// move a running chat off whatever model it was spawned on.
    #[wasm_bindgen_test]
    async fn editing_a_live_agents_setting_sends_it_to_that_agent() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let state = AppState::new();
        seed_host(&state);
        let active = seed_agent(&state);
        state.agent_session_settings.update(|map| {
            map.insert(
                active.as_agent_ref(),
                SessionSettingsValues(
                    [(
                        "profile".to_owned(),
                        SessionSettingValue::String("fast".to_owned()),
                    )]
                    .into_iter()
                    .collect(),
                ),
            );
        });
        mount_sheet(&container, state.clone());
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='session-settings-sheet']")
                .unwrap()
                .is_none(),
            "the sheet stays closed until the overflow menu asks for it"
        );

        state.session_settings_open.set(true);
        next_tick().await;

        let profile = control(&container, "profile");
        assert_eq!(
            profile.value(),
            "fast",
            "the control must show the setting the agent actually carries"
        );

        profile.set_value("deep");
        profile
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        next_tick().await;

        let lines = crate::bridge::test_sent_lines();
        assert_eq!(lines.len(), 1, "one edit is one frame, got: {lines:?}");
        let envelope: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(envelope["kind"], "set_session_settings");
        assert_eq!(
            envelope["stream"], "/agent/settings/inst",
            "the change must go to the edited agent's own instance stream"
        );
        assert_eq!(
            envelope["payload"]["values"]["profile"],
            serde_json::json!({ "string": "deep" }),
            "the edited value must be in the frame: {envelope}"
        );

        // The server is what decides: when its echo lands, the control shows
        // the settings the agent now actually carries.
        state.agent_session_settings.update(|map| {
            map.insert(
                active.as_agent_ref(),
                SessionSettingsValues(
                    [(
                        "profile".to_owned(),
                        SessionSettingValue::String("deep".to_owned()),
                    )]
                    .into_iter()
                    .collect(),
                ),
            );
        });
        next_tick().await;
        assert_eq!(
            control(&container, "profile").value(),
            "deep",
            "the echoed settings must be what the control reports"
        );
    }

    /// **An edit that never left the client must not sit on screen looking
    /// applied.**
    ///
    /// A touched `<select>` holds the user's own pick in the DOM, and a failed
    /// send changes nothing in the reactive values — so the control would go on
    /// reporting a model this chat is not running, on a surface whose entire
    /// job is to say which model a paid chat is running.
    #[wasm_bindgen_test]
    async fn a_rejected_edit_reverts_the_control_and_says_so() {
        let _guard = crate::bridge::test_reject_sends();
        let container = make_container();
        let state = AppState::new();
        seed_host(&state);
        let active = seed_agent(&state);
        state.agent_session_settings.update(|map| {
            map.insert(
                active.as_agent_ref(),
                SessionSettingsValues(
                    [(
                        "profile".to_owned(),
                        SessionSettingValue::String("fast".to_owned()),
                    )]
                    .into_iter()
                    .collect(),
                ),
            );
        });
        mount_sheet(&container, state.clone());
        state.session_settings_open.set(true);
        next_tick().await;

        let profile = control(&container, "profile");
        profile.set_value("deep");
        profile
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        next_tick().await;
        next_tick().await;

        assert_eq!(
            control(&container, "profile").value(),
            "fast",
            "a change that was never admitted must not be displayed as the setting"
        );
        let surfaced = state
            .mobile_shell_error
            .get_untracked()
            .expect("a setting change that was never sent must not fail silently");
        assert!(
            surfaced.message.contains("previous settings"),
            "the user must be told the chat is still on its old settings, got: {}",
            surfaced.message
        );
    }

    /// **A value the schema no longer offers is still what the session is on,
    /// and the sheet says so instead of silently showing something else.**
    ///
    /// Dropping it would make the control display the first option the browser
    /// picks — a chat on a withdrawn model would read as being on `Haiku`.
    #[wasm_bindgen_test]
    async fn a_withdrawn_value_stays_visible_and_is_not_selectable() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let state = AppState::new();
        seed_host(&state);
        let active = seed_agent(&state);
        state.agent_session_settings.update(|map| {
            map.insert(
                active.as_agent_ref(),
                SessionSettingsValues(
                    [(
                        "profile".to_owned(),
                        SessionSettingValue::String("retired".to_owned()),
                    )]
                    .into_iter()
                    .collect(),
                ),
            );
        });
        mount_sheet(&container, state.clone());
        state.session_settings_open.set(true);
        next_tick().await;

        let profile = control(&container, "profile");
        assert_eq!(
            profile.value(),
            "retired",
            "the session's real value must be the selected one"
        );
        let options = profile.query_selector_all("option").unwrap();
        let mut retired_label = None;
        for index in 0..options.length() {
            let option: web_sys::Element = options.item(index).unwrap().dyn_into().unwrap();
            if option.get_attribute("value").as_deref() == Some("retired") {
                assert!(
                    option.has_attribute("disabled"),
                    "a value the backend cannot serve must not be re-selectable"
                );
                retired_label = option.text_content();
            }
        }
        assert_eq!(
            retired_label.as_deref(),
            Some("retired (unavailable)"),
            "the entry must say why it is there"
        );
    }

    /// **A draft's edits are kept for the spawn, not sent to a chat that does
    /// not exist yet — and a dependent value the new parent invalidates is
    /// dropped rather than carried into the spawn.**
    #[wasm_bindgen_test]
    async fn a_draft_edit_is_held_for_the_spawn_and_clears_stale_dependents() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let state = AppState::new();
        seed_host(&state);
        state.host_settings_by_host.update(|map| {
            map.insert(
                host_id(),
                HostSettings {
                    enabled_backends: vec![BackendKind::Claude],
                    default_backend: Some(BackendKind::Claude),
                    ..HostSettings::default()
                },
            );
        });
        state.draft_session_settings.set(SessionSettingsValues(
            [
                (
                    "profile".to_owned(),
                    SessionSettingValue::String("fast".to_owned()),
                ),
                (
                    "model".to_owned(),
                    SessionSettingValue::String("haiku".to_owned()),
                ),
            ]
            .into_iter()
            .collect(),
        ));
        mount_sheet(&container, state.clone());
        state.session_settings_open.set(true);
        next_tick().await;

        let profile = control(&container, "profile");
        profile.set_value("deep");
        profile
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            0,
            "there is no agent to send to yet — the draft is carried by the spawn"
        );
        let draft = state.draft_session_settings.get_untracked();
        assert_eq!(
            draft.0.get("profile"),
            Some(&SessionSettingValue::String("deep".to_owned())),
            "the draft must carry the edit into the spawn"
        );
        assert_eq!(
            draft.0.get("model"),
            Some(&SessionSettingValue::Null),
            "a model the new profile does not offer must not be spawned with"
        );
    }
}
