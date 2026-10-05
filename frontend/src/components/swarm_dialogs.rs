//! Swarm creation, draft review, team conversion, and live change dialogs.
//!
//! Constraints are edited locally and sent as one typed command; the server
//! returns the resulting draft or change preview, which is rendered verbatim.
//! Launch and apply always name the exact revision the user reviewed. Limits
//! come from the canonical protocol constants the host validates against.

use std::collections::HashMap;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use protocol::{
    BackendKind, LaunchProfile, LaunchProfileEntry, LaunchProfileId, ProjectId,
    SWARM_MAX_AGENT_WAKE_BUDGET, SWARM_MAX_LIVE_AGENTS, SessionSchemaEntry, SessionSettingsValues,
    SwarmBackendAllocation, SwarmCommandPayload, SwarmConstraints, SwarmDraft, SwarmDraftId,
    SwarmId, SwarmMemberId, SwarmMemberSpec, SwarmRetirementPolicy, SwarmWorkspacePolicy,
};

use crate::components::session_settings::SessionSettingsControls;
use crate::components::swarm_view::{
    avatar_style, backend_name, initials, member_status_label, send_swarm_command,
    swarm_error_presentation,
};
use crate::components::swarms_panel::DraftDialogTarget;
use crate::state::{AppState, ProjectInfo, SwarmErrorEntry};

const FOCUSABLE: &str = "button:not([disabled]), input:not([disabled]), select:not([disabled]), \
    textarea:not([disabled]), summary, [href], [tabindex]:not([tabindex='-1'])";

fn focusable_elements(root: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    let Ok(list) = root.query_selector_all(FOCUSABLE) else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|index| list.item(index))
        .filter_map(|node| node.dyn_into::<web_sys::HtmlElement>().ok())
        .filter(|element| {
            let rect = element.get_bounding_client_rect();
            rect.width() > 0.0 || rect.height() > 0.0
        })
        .collect()
}

/// Modal shell: initial focus on the first control, Tab/Shift+Tab stay
/// inside, Escape closes, and focus returns to whatever opened it. No backdrop
/// dismiss: these forms carry unsaved constraints.
#[component]
fn SwarmModal(
    on_close: Callback<()>,
    labelled_by: &'static str,
    children: Children,
) -> impl IntoView {
    let modal_ref = NodeRef::<leptos::html::Div>::new();
    let opener: StoredValue<Option<web_sys::HtmlElement>, LocalStorage> = StoredValue::new_local(
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element())
            .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok()),
    );
    Effect::new(move |_| {
        if let Some(modal) = modal_ref.get() {
            match focusable_elements(&modal).into_iter().next() {
                Some(element) => {
                    let _ = element.focus();
                }
                None => {
                    let _ = modal.focus();
                }
            }
        }
    });
    on_cleanup(move || {
        if let Some(element) = opener.get_value().filter(|element| element.is_connected()) {
            let _ = element.focus();
        }
    });
    let on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Escape" => {
            ev.stop_propagation();
            ev.prevent_default();
            on_close.run(());
        }
        "Tab" => {
            let Some(modal) = modal_ref.get_untracked() else {
                return;
            };
            let items = focusable_elements(&modal);
            let (Some(first), Some(last)) = (items.first(), items.last()) else {
                ev.prevent_default();
                return;
            };
            let active = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.active_element());
            let at = |element: &web_sys::HtmlElement| {
                active
                    .as_ref()
                    .is_some_and(|active| active == element.unchecked_ref::<web_sys::Element>())
            };
            let inside = active.as_ref().is_some_and(|active| {
                modal.contains(Some(active.unchecked_ref::<web_sys::Node>()))
            });
            if ev.shift_key() && (at(first) || !inside) {
                ev.prevent_default();
                let _ = last.focus();
            } else if !ev.shift_key() && (at(last) || !inside) {
                ev.prevent_default();
                let _ = first.focus();
            }
        }
        _ => {}
    };
    view! {
        <div class="swarm-modal-overlay" on:keydown=on_keydown>
            <div
                class="swarm-modal"
                node_ref=modal_ref
                role="dialog"
                aria-modal="true"
                aria-labelledby=labelled_by
                tabindex="-1"
            >
                {children()}
            </div>
        </div>
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AllocationRow {
    key: u64,
    backend_kind: Option<BackendKind>,
    launch_profile_id: Option<LaunchProfileId>,
    session_settings: SessionSettingsValues,
    count: String,
}

/// Locally edited constraints. Nothing here is server state: it becomes a
/// `SwarmConstraints` only when the user asks the host for a preview.
#[derive(Clone, Copy)]
struct ConstraintsForm {
    name: RwSignal<String>,
    project_id: RwSignal<Option<ProjectId>>,
    policy: RwSignal<SwarmWorkspacePolicy>,
    max_live: RwSignal<String>,
    allocations: RwSignal<Vec<AllocationRow>>,
    guidance: RwSignal<String>,
    wake_budget: RwSignal<String>,
    next_key: RwSignal<u64>,
}

impl ConstraintsForm {
    fn new() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            project_id: RwSignal::new(None),
            policy: RwSignal::new(SwarmWorkspacePolicy::ReadOnly),
            max_live: RwSignal::new("4".to_owned()),
            allocations: RwSignal::new(Vec::new()),
            guidance: RwSignal::new(String::new()),
            wake_budget: RwSignal::new(String::new()),
            next_key: RwSignal::new(0),
        }
    }

    fn load(&self, constraints: &SwarmConstraints) {
        self.project_id.set(Some(constraints.project_id.clone()));
        self.policy.set(constraints.workspace_policy);
        self.max_live.set(constraints.max_live_agents.to_string());
        self.guidance.set(constraints.shared_guidance.clone());
        self.wake_budget.set(
            constraints
                .agent_wake_budget
                .map(|budget| budget.to_string())
                .unwrap_or_default(),
        );
        let rows = constraints
            .allocations
            .iter()
            .enumerate()
            .map(|(index, allocation)| AllocationRow {
                key: index as u64,
                backend_kind: Some(allocation.backend_kind),
                launch_profile_id: Some(allocation.launch_profile_id.clone()),
                session_settings: allocation.session_settings.clone(),
                count: allocation.count.to_string(),
            })
            .collect::<Vec<_>>();
        self.next_key.set(rows.len() as u64);
        self.allocations.set(rows);
    }

    fn add_row(&self, profile: Option<&LaunchProfile>, count: u32) {
        let key = self.next_key.get_untracked();
        self.next_key.set(key + 1);
        self.allocations.update(|rows| {
            rows.push(AllocationRow {
                key,
                backend_kind: profile.map(|profile| profile.backend_kind),
                launch_profile_id: profile.map(|profile| profile.id.clone()),
                session_settings: SessionSettingsValues::default(),
                count: count.to_string(),
            })
        });
    }

    fn constraints(&self) -> Result<SwarmConstraints, String> {
        let project_id = self.project_id.get_untracked().ok_or("Choose a project.")?;
        if self.policy.get_untracked().writable_consent() == Some(false) {
            return Err("Confirm write access to the selected scope.".to_owned());
        }
        let allocations = self
            .allocations
            .get_untracked()
            .into_iter()
            .map(|row| match (row.backend_kind, row.launch_profile_id) {
                (Some(backend_kind), Some(launch_profile_id)) => Ok(SwarmBackendAllocation {
                    session_settings: row.session_settings,
                    backend_kind,
                    launch_profile_id,
                    count: parse_number(
                        &row.count,
                        SWARM_MAX_LIVE_AGENTS,
                        "Agents from this profile",
                    )?,
                }),
                _ => Err("Choose a launch profile for every backend row.".to_owned()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if allocations.is_empty() {
            return Err("Add at least one backend.".to_owned());
        }
        Ok(SwarmConstraints {
            project_id,
            workspace_policy: self.policy.get_untracked(),
            max_live_agents: parse_number(
                &self.max_live.get_untracked(),
                SWARM_MAX_LIVE_AGENTS,
                "Live agents",
            )?,
            allocations,
            shared_guidance: self.guidance.get_untracked(),
            agent_wake_budget: parse_wake_budget(&self.wake_budget.get_untracked())?,
        })
    }

    fn numeric_error(&self) -> Option<String> {
        let max_live = self.max_live.get();
        let wake_budget = self.wake_budget.get();
        let allocations = self.allocations.get();
        parse_number(&max_live, SWARM_MAX_LIVE_AGENTS, "Live agents")
            .err()
            .or_else(|| parse_wake_budget(&wake_budget).err())
            .or_else(|| {
                allocations.iter().find_map(|row| {
                    parse_number(
                        &row.count,
                        SWARM_MAX_LIVE_AGENTS,
                        "Agents from this profile",
                    )
                    .err()
                })
            })
    }

    fn track_constraints(&self) {
        self.project_id.track();
        self.policy.track();
        self.max_live.track();
        self.allocations.track();
        self.guidance.track();
        self.wake_budget.track();
    }
}

fn ready_profiles(entries: &[LaunchProfileEntry]) -> Vec<LaunchProfile> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            LaunchProfileEntry::Ready { profile } => Some(profile.clone()),
            LaunchProfileEntry::Unavailable { .. } => None,
        })
        .collect()
}

/// The schema a launch profile's settings are validated against: a custom
/// profile's own schema when the catalog lists one, else its backend's.
fn profile_schema(
    state: &AppState,
    host_id: &str,
    profile_id: &LaunchProfileId,
    backend: BackendKind,
) -> Option<SessionSchemaEntry> {
    let custom = state.launch_profile_catalog.with(|catalogs| {
        catalogs.get(host_id).and_then(|catalog| {
            catalog
                .custom_profile_schemas
                .iter()
                .find(|entry| &entry.launch_profile_id == profile_id)
                .map(|entry| entry.schema.clone())
        })
    });
    custom.or_else(|| {
        state.session_schemas.with(|schemas| {
            schemas
                .get(host_id)
                .and_then(|by_backend| by_backend.get(&backend).cloned())
        })
    })
}

/// Model settings for one allocation or member, rendered from the
/// server-owned schema. Values override the launch profile's settings.
#[component]
fn SettingsOverrides(
    host: StoredValue<String>,
    profile_id: LaunchProfileId,
    backend: BackendKind,
    values: Signal<SessionSettingsValues>,
    on_change: Callback<SessionSettingsValues>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_sv = StoredValue::new_local(state);
    let profile_id = StoredValue::new(profile_id);
    let schema: Memo<Option<SessionSchemaEntry>> = Memo::new(move |_| {
        state_sv.with_value(|state| {
            profile_schema(state, &host.get_value(), &profile_id.get_value(), backend)
        })
    });
    move || {
        match schema.get() {
        Some(SessionSchemaEntry::Ready { schema }) if schema.fields.is_empty() => view! {
            <span class="swarm-field-help">"This backend has no model settings."</span>
        }
        .into_any(),
        Some(SessionSchemaEntry::Ready { schema }) => view! {
            <SessionSettingsControls schema=schema values=values on_change=on_change />
        }
        .into_any(),
        Some(SessionSchemaEntry::Pending { .. }) => view! {
            <span class="swarm-field-help" role="status">"Loading model options from the host…"</span>
        }
        .into_any(),
        Some(SessionSchemaEntry::Unavailable { message, .. }) => {
            let text = if message.trim().is_empty() {
                "The host could not load settings for this backend.".to_owned()
            } else {
                message
            };
            view! { <span class="swarm-field-help" role="status">{text}</span> }.into_any()
        }
        None => view! {
            <span class="swarm-field-help">"The host has not reported settings for this backend."</span>
        }
        .into_any(),
    }
    }
}

/// An empty limit means agents may wake each other without limit.
fn parse_wake_budget(value: &str) -> Result<Option<u32>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    parse_number(
        value,
        SWARM_MAX_AGENT_WAKE_BUDGET,
        "Agent-to-agent turn limit",
    )
    .map(Some)
}

fn parse_number(value: &str, max: u32, label: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| (1..=max).contains(value))
        .ok_or_else(|| format!("{label} must be a whole number from 1 to {max}."))
}

/// The constraints half of the create and manage dialogs.
#[component]
fn ConstraintsFields(
    host: StoredValue<String>,
    form: ConstraintsForm,
    /// A running swarm keeps its project and workspace policy.
    #[prop(optional)]
    locked_scope: bool,
    #[prop(optional)] show_identity: bool,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let projects_signal = state.projects;
    let catalog_signal = state.launch_profile_catalog;

    let projects: Memo<Vec<ProjectInfo>> = Memo::new(move |_| {
        projects_signal.with(|projects| {
            projects
                .iter()
                .filter(|info| info.host_id == host.get_value())
                .cloned()
                .collect()
        })
    });
    let profiles: Memo<Vec<LaunchProfile>> = Memo::new(move |_| {
        catalog_signal.with(|catalogs| {
            catalogs
                .get(&host.get_value())
                .map(|catalog| ready_profiles(&catalog.entries))
                .unwrap_or_default()
        })
    });
    let selected_is_workbench = Memo::new(move |_| {
        let selected = form.project_id.get();
        projects.with(|projects| {
            projects.iter().any(|info| {
                Some(&info.project.id) == selected.as_ref() && info.project.is_workbench()
            })
        })
    });
    Effect::new(move |_| {
        if !locked_scope
            && !selected_is_workbench.get()
            && matches!(
                form.policy.get_untracked(),
                SwarmWorkspacePolicy::SharedWorkbench { .. }
            )
        {
            form.policy.set(SwarmWorkspacePolicy::ReadOnly);
        }
    });
    let allocated = Memo::new(move |_| {
        form.allocations.with(|rows| {
            rows.iter()
                .map(|row| {
                    parse_number(
                        &row.count,
                        SWARM_MAX_LIVE_AGENTS,
                        "Agents from this profile",
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|counts| counts.into_iter().sum::<u32>())
        })
    });

    view! {
        <div class="swarm-form">
            {move || form.numeric_error().map(|message| view! { <div class="swarm-banner" data-tone="error" role="alert"><span class="swarm-banner-text">{message}</span></div> })}
            {show_identity.then(|| view! {
                <label class="swarm-field">
                    <span class="swarm-field-label">"Name"</span>
                    <input
                        class="swarm-input"
                        type="text"
                        data-field="name"
                        placeholder="e.g. Checkout reliability"
                        prop:value=move || form.name.get()
                        on:input=move |ev| form.name.set(event_target_value(&ev))
                    />
                </label>
            })}
            <label class="swarm-field">
                <span class="swarm-field-label">{move || match form.policy.get() {
                    SwarmWorkspacePolicy::SharedHost { .. } => "Starting project",
                    SwarmWorkspacePolicy::ReadOnly
                    | SwarmWorkspacePolicy::SharedProject { .. }
                    | SwarmWorkspacePolicy::SharedWorkbench { .. } => "Project",
                }}</span>
                <select
                    class="swarm-input"
                    data-field="project"
                    disabled=locked_scope
                    on:change=move |ev| {
                        let value = event_target_value(&ev);
                        form.project_id.set((!value.is_empty()).then_some(ProjectId(value)));
                    }
                >
                    <option value="" selected=move || form.project_id.get().is_none()>"Choose a project…"</option>
                    {move || projects.get().into_iter().map(|info| {
                        let id = info.project.id.clone();
                        let label = if info.project.is_workbench() {
                            format!("{} (workbench)", info.project.name)
                        } else {
                            info.project.name.clone()
                        };
                        let id_for_selected = id.clone();
                        view! {
                            <option
                                value=id.0.clone()
                                selected=move || form.project_id.get().as_ref() == Some(&id_for_selected)
                                disabled=move || info.project.is_workbench()
                                    && matches!(form.policy.get(), SwarmWorkspacePolicy::SharedProject { .. })
                            >
                                {label}
                            </option>
                        }
                    }).collect_view()}
                </select>
            </label>
            <fieldset class="swarm-field swarm-policy" disabled=locked_scope>
                <legend class="swarm-field-label">"Workspace access"</legend>
                <label class="swarm-radio">
                    <input
                        type="radio"
                        name="swarm-policy"
                        data-policy="shared_host"
                        prop:checked=move || matches!(form.policy.get(), SwarmWorkspacePolicy::SharedHost { .. })
                        on:change=move |_| form.policy.set(SwarmWorkspacePolicy::SharedHost { writable_consent: false })
                    />
                    <span>
                        <span class="swarm-radio-title">"Host scope — writable"</span>
                        <span class="swarm-field-help">"Members can edit all projects and workbenches on this host, including ones added later."</span>
                    </span>
                </label>
                <label class="swarm-radio">
                    <input
                        type="radio"
                        name="swarm-policy"
                        data-policy="shared_project"
                        prop:checked=move || matches!(form.policy.get(), SwarmWorkspacePolicy::SharedProject { .. })
                        on:change=move |_| {
                            if selected_is_workbench.get_untracked() {
                                let selected = form.project_id.get_untracked();
                                let parent = projects.get_untracked().iter()
                                    .find(|info| Some(&info.project.id) == selected.as_ref())
                                    .and_then(|info| info.project.parent_project_id().cloned());
                                form.project_id.set(parent);
                            }
                            form.policy.set(SwarmWorkspacePolicy::SharedProject { writable_consent: true });
                        }
                    />
                    <span>
                        <span class="swarm-radio-title">"Project scope — writable"</span>
                        <span class="swarm-field-help">"Members can edit the selected project and all its workbenches, including newly created workbenches."</span>
                    </span>
                </label>
                <label class="swarm-radio">
                    <input
                        type="radio"
                        name="swarm-policy"
                        data-policy="read_only"
                        prop:checked=move || form.policy.get() == SwarmWorkspacePolicy::ReadOnly
                        on:change=move |_| form.policy.set(SwarmWorkspacePolicy::ReadOnly)
                    />
                    <span>
                        <span class="swarm-radio-title">"Read-only project access"</span>
                        <span class="swarm-field-help">"Members read and discuss; nobody edits files."</span>
                    </span>
                </label>
                <label class="swarm-radio" class:disabled=move || !selected_is_workbench.get()>
                    <input
                        type="radio"
                        name="swarm-policy"
                        data-policy="shared_workbench"
                        disabled=move || !selected_is_workbench.get()
                        prop:checked=move || matches!(form.policy.get(), SwarmWorkspacePolicy::SharedWorkbench { .. })
                        on:change=move |_| form.policy.set(SwarmWorkspacePolicy::SharedWorkbench { writable_consent: true })
                    />
                    <span>
                        <span class="swarm-radio-title">"Workbench scope — writable"</span>
                        <span class="swarm-field-help">
                            {move || if selected_is_workbench.get() {
                                "Members can edit this workbench. Nothing is landed on main automatically."
                            } else {
                                "Select a workbench to limit writes to that working tree."
                            }}
                        </span>
                    </span>
                </label>
                <Show when=move || matches!(form.policy.get(), SwarmWorkspacePolicy::SharedHost { .. })>
                    <div class="swarm-banner" data-tone="warn" role="note">
                        <span class="swarm-banner-text">
                            "Members can edit files in this scope. Edits are not serialized: two members can change the same file at the same time. Repository rules still apply."
                        </span>
                    </div>
                    <label class="swarm-consent">
                        <input
                            type="checkbox"
                            data-field="writable-consent"
                            prop:checked=move || form.policy.get().writable_consent() == Some(true)
                            on:change=move |ev| {
                                let writable_consent = event_target_checked(&ev);
                                form.policy.update(|policy| match policy {
                                    SwarmWorkspacePolicy::ReadOnly => {},
                                    SwarmWorkspacePolicy::SharedWorkbench { writable_consent: consent }
                                    | SwarmWorkspacePolicy::SharedProject { writable_consent: consent }
                                    | SwarmWorkspacePolicy::SharedHost { writable_consent: consent } => *consent = writable_consent,
                                });
                            }
                        />
                        <span>"I allow members to write to all projects and workbenches on this host."</span>
                    </label>
                </Show>
            </fieldset>
            <div class="swarm-field">
                <span class="swarm-field-label" id="swarm-live-label">"Live agents"</span>
                <div class="swarm-stepper">
                    <button
                        class="swarm-btn swarm-stepper-btn"
                        aria-label="Fewer live agents"
                        disabled=move || !parse_number(&form.max_live.get(), SWARM_MAX_LIVE_AGENTS, "Live agents").is_ok_and(|value| value > 1)
                        on:click=move |_| if let Ok(value) = parse_number(&form.max_live.get_untracked(), SWARM_MAX_LIVE_AGENTS, "Live agents") { form.max_live.set(value.saturating_sub(1).max(1).to_string()); }
                    >
                        "−"
                    </button>
                    <input
                        class="swarm-input swarm-stepper-input"
                        type="number"
                        min="1"
                        max=SWARM_MAX_LIVE_AGENTS.to_string()
                        data-field="max-live"
                        aria-invalid=move || parse_number(&form.max_live.get(), SWARM_MAX_LIVE_AGENTS, "Live agents").is_err().to_string()
                        aria-labelledby="swarm-live-label"
                        prop:value=move || form.max_live.get()
                        on:input=move |ev| form.max_live.set(event_target_value(&ev))
                        on:change=move |ev| form.max_live.set(event_target_value(&ev))
                    />
                    <button
                        class="swarm-btn swarm-stepper-btn"
                        aria-label="More live agents"
                        disabled=move || !parse_number(&form.max_live.get(), SWARM_MAX_LIVE_AGENTS, "Live agents").is_ok_and(|value| value < SWARM_MAX_LIVE_AGENTS)
                        on:click=move |_| if let Ok(value) = parse_number(&form.max_live.get_untracked(), SWARM_MAX_LIVE_AGENTS, "Live agents") { form.max_live.set((value + 1).min(SWARM_MAX_LIVE_AGENTS).to_string()); }
                    >
                        "+"
                    </button>
                    <span class="swarm-field-help">
                        {move || allocated.with(|allocated| match allocated { Ok(count) => format!("{count} allocated across backends"), Err(_) => "Correct backend counts.".to_owned() })}
                    </span>
                </div>
            </div>
            <div class="swarm-field">
                <span class="swarm-field-label">"Backends and models"</span>
                <div class="swarm-allocations">
                    <For
                        each={move || {
                            form.allocations
                                .get()
                                .into_iter()
                                .map(|row| row.key)
                                .collect::<Vec<_>>()
                        }}
                        key={|key| *key}
                        let:row_key
                    >
                        <AllocationEditor host=host form=form row_key=row_key profiles=profiles />
                    </For>
                    <div class="swarm-allocation-actions">
                        <button
                            class="swarm-btn"
                            disabled=move || profiles.with(|profiles| profiles.is_empty())
                            on:click=move |_| {
                                let first = profiles.with_untracked(|profiles| profiles.first().cloned());
                                form.add_row(first.as_ref(), 1);
                            }
                        >
                            "+ Add backend"
                        </button>
                        <span class="swarm-field-help">
                            {move || if profiles.with(|profiles| profiles.is_empty()) {
                                "No launch profiles are ready on this host. Enable a backend in Settings."
                            } else {
                                "Model choices come from each backend. Unset fields use the launch profile's settings."
                            }}
                        </span>
                    </div>
                </div>
            </div>
            {(!show_identity).then(|| view! {
                <details class="swarm-advanced">
                    <summary>"Advanced settings"</summary>
                    <div class="swarm-advanced-fields">
            <label class="swarm-field">
                <span class="swarm-field-label">"Standing instructions (optional)"</span>
                <textarea
                    class="swarm-input swarm-textarea"
                    rows="3"
                    data-field="guidance"
                    placeholder="Ongoing rules for all agents, such as how to test or format code."
                    prop:value=move || form.guidance.get()
                    on:input=move |ev| form.guidance.set(event_target_value(&ev))
                ></textarea>
            </label>
            <label class="swarm-field">
                <span class="swarm-field-label">"Agent-to-agent turn limit (optional)"</span>
                <span class="swarm-field-row">
                    <input
                        class="swarm-input swarm-stepper-input"
                        type="number"
                        min="1"
                        max=SWARM_MAX_AGENT_WAKE_BUDGET.to_string()
                        data-field="wake-budget"
                        placeholder="No limit"
                        aria-invalid=move || parse_wake_budget(&form.wake_budget.get()).is_err().to_string()
                        prop:value=move || form.wake_budget.get()
                        on:input=move |ev| form.wake_budget.set(event_target_value(&ev))
                        on:change=move |ev| form.wake_budget.set(event_target_value(&ev))
                    />
                    <span class="swarm-field-help">"Off by default. When set, limits agent-to-agent turns in each request you start; at the limit, new turns stop until you choose Resume."</span>
                </span>
            </label>
                    </div>
                </details>
            })}
        </div>
    }
}

#[component]
fn AllocationEditor(
    host: StoredValue<String>,
    form: ConstraintsForm,
    row_key: u64,
    profiles: Memo<Vec<LaunchProfile>>,
) -> impl IntoView {
    let row = Memo::new(move |_| {
        form.allocations
            .with(|rows| rows.iter().find(|row| row.key == row_key).cloned())
    });
    let update = move |change: &dyn Fn(&mut AllocationRow)| {
        form.allocations.update(|rows| {
            if let Some(row) = rows.iter_mut().find(|row| row.key == row_key) {
                change(row);
            }
        });
    };
    let selection = Memo::new(move |_| {
        row.with(|row| {
            row.as_ref()
                .and_then(|row| row.launch_profile_id.clone().zip(row.backend_kind))
        })
    });
    view! {
        <div class="swarm-allocation-row">
            <div class="swarm-allocation-line">
                <select
                    class="swarm-input swarm-allocation-profile"
                    aria-label="Launch profile"
                    on:change=move |ev| {
                        let value = event_target_value(&ev);
                        let backend = profiles.with_untracked(|profiles| {
                            profiles.iter().find(|profile| profile.id.0 == value).map(|profile| profile.backend_kind)
                        });
                        update(&|row: &mut AllocationRow| {
                            row.launch_profile_id = (!value.is_empty()).then(|| LaunchProfileId(value.clone()));
                            row.backend_kind = backend;
                            // Settings belong to the previous profile's schema.
                            row.session_settings = SessionSettingsValues::default();
                        });
                    }
                >
                    <option value="" selected=move || selection.get().is_none()>"Choose…"</option>
                    {move || {
                        let current = selection.get().map(|(id, _)| id);
                        let missing = current
                            .clone()
                            .filter(|id| profiles.with(|profiles| !profiles.iter().any(|p| &p.id == id)))
                            .map(|id| view! {
                                <option value=id.0.clone() selected=true>{format!("{} (unavailable)", id.0)}</option>
                            });
                        let options = profiles.get().into_iter().map(|profile| {
                            let selected = current.as_ref() == Some(&profile.id);
                            view! {
                                <option value=profile.id.0.clone() selected=selected>
                                    {format!("{} · {}", backend_name(profile.backend_kind), profile.label)}
                                </option>
                            }
                        }).collect_view();
                        view! { {missing} {options} }
                    }}
                </select>
                <input
                    class="swarm-input swarm-allocation-count"
                    type="number"
                    min="1"
                    max=SWARM_MAX_LIVE_AGENTS.to_string()
                    aria-label="Agents from this profile"
                    aria-invalid=move || row.with(|row| row.as_ref().is_none_or(|row| parse_number(&row.count, SWARM_MAX_LIVE_AGENTS, "Agents from this profile").is_err())).to_string()
                    prop:value=move || row.with(|row| row.as_ref().map(|row| row.count.clone()).unwrap_or_default())
                    on:input=move |ev| {
                        let value = event_target_value(&ev);
                        update(&|row: &mut AllocationRow| row.count = value.clone());
                    }
                    on:change=move |ev| {
                        let value = event_target_value(&ev);
                        update(&|row: &mut AllocationRow| row.count = value.clone());
                    }
                />
                <button
                    class="swarm-btn swarm-btn-quiet"
                    aria-label="Remove this backend"
                    on:click=move |_| form.allocations.update(|rows| rows.retain(|row| row.key != row_key))
                >
                    "Remove"
                </button>
            </div>
            {move || selection.get().map(|(profile_id, backend)| view! {
                <div class="swarm-allocation-settings">
                    <SettingsOverrides
                        host=host
                        profile_id=profile_id
                        backend=backend
                        values=Signal::derive(move || row.with(|row| row.as_ref().map(|row| row.session_settings.clone()).unwrap_or_default()))
                        on_change=Callback::new(move |values: SessionSettingsValues| {
                            update(&|row: &mut AllocationRow| row.session_settings = values.clone());
                        })
                    />
                </div>
            })}
        </div>
    }
}

fn errors_for(
    errors: &[SwarmErrorEntry],
    host_id: &str,
    matches: impl Fn(&SwarmErrorEntry) -> bool,
) -> Vec<SwarmErrorEntry> {
    errors
        .iter()
        .filter(|entry| entry.host_id == host_id && matches(entry))
        .cloned()
        .collect()
}

#[component]
fn ErrorList(errors: Signal<Vec<SwarmErrorEntry>>) -> impl IntoView {
    let errors_signal = expect_context::<AppState>().swarm_errors;
    view! {
        <For each=move || errors.get() key=|entry| (entry.host_id.clone(), entry.serial) let:entry>
            {
                let serial = entry.serial;
                let host_id = entry.host_id.clone();
                let (tone, role) = swarm_error_presentation(entry.error.code);
                view! {
                    <div class="swarm-banner" data-tone=tone role=role>
                        <span class="swarm-banner-text">{entry.error.message.clone()}</span>
                        <button
                            class="swarm-btn swarm-btn-quiet"
                            on:click=move |_| errors_signal.update(|errors| errors.retain(|e| !(e.serial == serial && e.host_id == host_id)))
                        >
                            "Dismiss"
                        </button>
                    </div>
                }
            }
        </For>
    }
}

type DraftMemberEditors = RwSignal<HashMap<SwarmMemberId, Memo<bool>>>;

// ── Create / review a draft ────────────────────────────────────────────────

#[component]
pub fn SwarmDraftDialog(
    host_id: String,
    target: DraftDialogTarget,
    on_close: Callback<()>,
    /// Called with the draft id once Launch/Convert is sent; the launched
    /// swarm names it in `source_draft_id`.
    on_launched: Callback<SwarmDraftId>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let host_streams = state.host_streams;
    let drafts_signal = state.swarm_drafts;
    let errors_signal = state.swarm_errors;
    let catalog_signal = state.launch_profile_catalog;
    let active_project = state.active_project;
    let host = StoredValue::new(host_id);
    let draft_id = StoredValue::new(target.draft_id().clone());
    let is_new = matches!(target, DraftDialogTarget::New(_));
    let form = ConstraintsForm::new();
    let form_error: RwSignal<Option<String>> = RwSignal::new(None);
    let member_editors: DraftMemberEditors = RwSignal::new(HashMap::new());
    // The draft revision a GenerateDraft was sent against; cleared when the
    // host answers with a different revision or an error.
    let generating: RwSignal<Option<Option<u64>>> = RwSignal::new(None);

    let draft: Memo<Option<SwarmDraft>> = Memo::new(move |_| {
        drafts_signal.with(|map| {
            map.get(&host.get_value())
                .and_then(|drafts| drafts.get(&draft_id.get_value()).cloned())
        })
    });
    let unsaved_members = Memo::new(move |_| {
        draft.with(|draft| {
            draft.as_ref().is_some_and(|draft| {
                member_editors.with(|editors| {
                    draft
                        .members
                        .iter()
                        .any(|member| editors.get(&member.id).is_none_or(|dirty| dirty.get()))
                })
            })
        })
    });
    let dialog_errors: Memo<Vec<SwarmErrorEntry>> = Memo::new(move |_| {
        errors_signal.with(|errors| {
            errors_for(errors, &host.get_value(), |entry| {
                entry.error.swarm_id.is_none()
                    && entry.error.draft_id.as_ref() == Some(&draft_id.get_value())
            })
        })
    });
    Effect::new(move |_| {
        let Some(sent_from) = generating.get() else {
            return;
        };
        let revision = draft.with(|draft| draft.as_ref().map(|draft| draft.revision));
        if revision != sent_from || !dialog_errors.with(|errors| errors.is_empty()) {
            generating.set(None);
        }
    });

    // Seed the form once: from the draft under review, else from the active
    // project and the host's default launch profile.
    let seeded = RwSignal::new(false);
    Effect::new(move |_| {
        if seeded.get_untracked() {
            return;
        }
        if !is_new {
            if let Some(current) = draft.get() {
                form.name.set(current.name.clone());
                form.load(&current.constraints);
                seeded.set(true);
            }
            return;
        }
        let default_profile = catalog_signal.with_untracked(|catalogs| {
            catalogs.get(&host.get_value()).and_then(|catalog| {
                let ready = ready_profiles(&catalog.entries);
                catalog
                    .default_profile_id
                    .as_ref()
                    .and_then(|id| ready.iter().find(|profile| &profile.id == id).cloned())
                    .or_else(|| ready.into_iter().next())
            })
        });
        if let Some(active) = active_project
            .get_untracked()
            .filter(|active| active.host_id == host.get_value())
        {
            form.project_id.set(Some(active.project_id));
        }
        if let Some(profile) = default_profile {
            form.add_row(Some(&profile), 2);
        }
        seeded.set(true);
    });

    let on_send_error = Callback::new(move |message: String| {
        generating.set(None);
        form_error.set(Some(format!("Could not reach the host: {message}")));
    });
    let send = move |command: SwarmCommandPayload| {
        form_error.set(None);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            command,
            Some(on_send_error),
        );
    };

    let generate = move |_| {
        let name = form.name.get_untracked();
        if name.trim().is_empty() {
            form_error.set(Some("Name the swarm.".to_owned()));
            return;
        }
        let constraints = match form.constraints() {
            Ok(constraints) => constraints,
            Err(message) => {
                form_error.set(Some(message));
                return;
            }
        };
        let revision = draft.with_untracked(|draft| draft.as_ref().map(|draft| draft.revision));
        let host_id = host.get_value();
        let id = draft_id.get_value();
        errors_signal.update(|errors| {
            errors.retain(|entry| {
                !(entry.host_id == host_id && entry.error.draft_id.as_ref() == Some(&id))
            })
        });
        generating.set(Some(revision));
        send(SwarmCommandPayload::GenerateDraft {
            draft_id: id,
            expected_revision: revision,
            name,
            constraints,
        });
    };

    let discard = move |_| {
        send(SwarmCommandPayload::DiscardDraft {
            draft_id: draft_id.get_value(),
        });
        on_close.run(());
    };

    // The form no longer matches the reviewed draft, so launching would not
    // launch what the user is looking at: a fresh preview is required first.
    let form_changed = Memo::new(move |_| {
        let Some(current) = draft.get() else {
            return true;
        };
        form.name.track();
        form.track_constraints();
        current.name != form.name.get_untracked()
            || form.constraints().ok().as_ref() != Some(&current.constraints)
    });

    let is_migration =
        move || draft.with(|d| d.as_ref().is_some_and(|d| d.legacy_team_id.is_some()));
    let title = move || match (is_new, is_migration()) {
        (_, true) => "Convert team to swarm",
        (false, false) => "Review swarm draft",
        (true, false) => "New swarm",
    };
    let launch_blocker = move || -> Option<&'static str> {
        if form.numeric_error().is_some() {
            return Some("Correct the numeric constraints before launching");
        }
        if generating.get().is_some() {
            return Some("Waiting for the preview");
        }
        if unsaved_members.get() {
            return Some("Save member edits before launching");
        }
        if draft.with(|draft| draft.is_some()) && form_changed.get() {
            return Some("Update the preview so you launch what you see");
        }
        draft.with(|draft| match draft {
            None => Some("Generate a preview first"),
            Some(draft) if !draft.conflicts.is_empty() => {
                Some("Resolve the conflicts listed in the preview")
            }
            Some(_) => None,
        })
    };

    let launch = move |_| {
        if launch_blocker().is_some() {
            return;
        }
        let Some(current) = draft.get_untracked() else {
            return;
        };
        let command = if current.legacy_team_id.is_some() {
            SwarmCommandPayload::ApplyMigration {
                draft_id: current.id.clone(),
                expected_revision: current.revision,
            }
        } else {
            SwarmCommandPayload::Launch {
                draft_id: current.id.clone(),
                expected_revision: current.revision,
            }
        };
        send(command);
        on_launched.run(current.id);
        on_close.run(());
    };

    view! {
        <SwarmModal on_close=on_close labelled_by="swarm-draft-title">
            <header class="swarm-modal-header">
                <h2 id="swarm-draft-title" class="swarm-modal-title">{title}</h2>
                <p class="swarm-modal-subtitle">
                    "Choose your agents, review the lineup, then start talking in Briefing. You can adjust advanced settings later in Manage."
                </p>
            </header>
            <div class="swarm-modal-body swarm-modal-split">
                <section class="swarm-modal-pane" aria-label="Constraints">
                    <ConstraintsFields host=host form=form show_identity=true />
                </section>
                <section
                    class="swarm-modal-pane swarm-preview-pane"
                    aria-label="Preview"
                    aria-busy=move || generating.get().is_some().to_string()
                >
                    <h3 class="swarm-pane-title">"Preview"</h3>
                    {move || generating.get().map(|_| view! {
                        <div class="swarm-loading" role="status">"Generating preview…"</div>
                    })}
                    <Show when=move || draft.with(|draft| draft.is_some()) fallback=|| view! {
                        <div class="swarm-empty">
                            "Preview the proposed agents before creating your swarm. They start when you send a message."
                        </div>
                    }>
                        <DraftPreview host=host draft=draft member_editors=member_editors />
                    </Show>
                </section>
            </div>
            <div class="swarm-modal-errors">
                {move || form_error.get().map(|message| view! {
                    <div class="swarm-banner" data-tone="error" role="alert"><span class="swarm-banner-text">{message}</span></div>
                })}
                <ErrorList errors=Signal::derive(move || dialog_errors.get()) />
            </div>
            <footer class="swarm-modal-footer">
                <span class="swarm-field-help swarm-footer-note">
                    {move || if unsaved_members.get() { Some("Some member edits are not saved on the host.") } else { draft.with(|draft| draft.is_some()).then_some("This draft is saved on the host.") }}
                </span>
                <Show when=move || draft.with(|draft| draft.is_some())>
                    <button class="swarm-btn swarm-btn-quiet swarm-btn-danger" on:click=discard>"Discard draft"</button>
                </Show>
                <button class="swarm-btn swarm-btn-quiet" on:click=move |_| on_close.run(())>"Close"</button>
                <button
                    class="swarm-btn"
                    class:swarm-btn-primary=move || form_changed.get()
                    data-action="generate"
                    disabled=move || generating.get().is_some() || form.numeric_error().is_some()
                    on:click=generate
                >
                    {move || if draft.with(|draft| draft.is_some()) { "Update preview" } else { "Generate preview" }}
                </button>
                <button
                    class="swarm-btn"
                    class:swarm-btn-primary=move || !form_changed.get()
                    data-action="launch"
                    disabled=move || launch_blocker().is_some()
                    title=move || launch_blocker().unwrap_or("")
                    on:click=launch
                >
                    {move || {
                        let revision = draft.with(|d| d.as_ref().map(|d| format!(" revision {}", d.revision)));
                        if is_migration() { format!("Convert{}", revision.unwrap_or_default()) } else { "Create swarm".to_owned() }
                    }}
                </button>
            </footer>
        </SwarmModal>
    }
}

#[component]
fn SwarmToolPolicyNote() -> impl IntoView {
    view! {
        <p class="swarm-field-help swarm-tool-policy" data-field="tool-policy">
            "Tools: members get the ten swarm board tools (including atomic thread state and delta tools) plus their backend's built-in tools. Writable host and project scopes also get scoped workbench listing, creation, and removal. Your other configured MCP servers are not attached; file writes follow the workspace access above."
        </p>
    }
}

#[component]
fn DraftPreview(
    host: StoredValue<String>,
    draft: Memo<Option<SwarmDraft>>,
    member_editors: DraftMemberEditors,
) -> impl IntoView {
    let member_ids = Memo::new(move |_| {
        draft.with(|draft| {
            draft
                .as_ref()
                .map(|draft| {
                    draft
                        .members
                        .iter()
                        .map(|member| member.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    view! {
        <div class="swarm-preview">
            <p class="swarm-field-help">
                {move || draft.with(|draft| draft.as_ref().map(|draft| {
                    let retained = draft.retained_sessions.len();
                    format!("Generalist peers, generated without a model · revision {}{}", draft.revision,
                        if retained > 0 { format!(" · {retained} existing conversations kept") } else { String::new() })
                }))}
            </p>
            <SwarmToolPolicyNote />
            {move || draft.with(|draft| draft.as_ref().filter(|draft| !draft.conflicts.is_empty()).map(|draft| view! {
                <div class="swarm-banner swarm-banner-list" data-tone="warn" role="status">
                    <span class="swarm-banner-text">"Resolve these before launching:"</span>
                    <ul>{draft.conflicts.iter().map(|conflict| view! { <li>{conflict.clone()}</li> }).collect_view()}</ul>
                </div>
            }))}
            <ul class="swarm-preview-members">
                <For each=move || member_ids.get() key=|id| id.clone() let:member_id>
                    <DraftMemberRow host=host draft=draft member_id=member_id member_editors=member_editors />
                </For>
            </ul>
        </div>
    }
}

#[component]
fn DraftMemberRow(
    host: StoredValue<String>,
    draft: Memo<Option<SwarmDraft>>,
    member_id: SwarmMemberId,
    member_editors: DraftMemberEditors,
) -> impl IntoView {
    let host_streams = expect_context::<AppState>().host_streams;
    let member_id = StoredValue::new(member_id);
    let member: Memo<Option<SwarmMemberSpec>> = Memo::new(move |_| {
        draft.with(|draft| {
            draft.as_ref().and_then(|draft| {
                draft
                    .members
                    .iter()
                    .find(|member| member.id == member_id.get_value())
                    .cloned()
            })
        })
    });
    let name_edit: RwSignal<Option<String>> = RwSignal::new(None);
    let focus_edit: RwSignal<Option<String>> = RwSignal::new(None);
    let settings_edit: RwSignal<Option<SessionSettingsValues>> = RwSignal::new(None);
    let settings_open = RwSignal::new(false);
    let send_error: RwSignal<Option<String>> = RwSignal::new(None);
    let name = Memo::new(move |_| {
        name_edit
            .get()
            .or_else(|| member.with(|member| member.as_ref().map(|member| member.name.clone())))
    });
    let focus = Memo::new(move |_| {
        focus_edit.get().or_else(|| {
            member.with(|member| {
                member
                    .as_ref()
                    .map(|member| member.focus.clone().unwrap_or_default())
            })
        })
    });
    let settings = Memo::new(move |_| {
        settings_edit.get().or_else(|| {
            member.with(|member| {
                member
                    .as_ref()
                    .map(|member| member.session_settings.clone())
            })
        })
    });
    let edited = Memo::new(move |_| {
        let mut current = member.get()?;
        current.name = name.get()?;
        if let Some(focus) = focus_edit.get() {
            current.focus = (!focus.trim().is_empty()).then_some(focus);
        }
        current.session_settings = settings.get()?;
        Some(current)
    });
    let dirty = Memo::new(move |_| edited.get() != member.get());
    member_editors.update(|editors| {
        editors.insert(member_id.get_value(), dirty);
    });
    on_cleanup(move || {
        member_editors.update(|editors| {
            editors.remove(&member_id.get_value());
        })
    });
    Effect::new(move |_| {
        let Some(current) = member.get() else {
            return;
        };
        if name_edit.get().as_ref() == Some(&current.name) {
            name_edit.set(None);
        }
        if focus_edit
            .get()
            .is_some_and(|focus| (!focus.trim().is_empty()).then_some(focus) == current.focus)
        {
            focus_edit.set(None);
        }
        if settings_edit.get().as_ref() == Some(&current.session_settings) {
            settings_edit.set(None);
        }
    });
    // Saved edits pin the member; only the server's matching record approves them.
    let edit = move |pinned: bool| {
        let (Some(current), Some(mut spec)) = (draft.get_untracked(), edited.get_untracked())
        else {
            send_error.set(Some("This member is no longer in the draft.".to_owned()));
            return;
        };
        spec.pinned = pinned;
        send_error.set(None);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            SwarmCommandPayload::EditDraftMember {
                draft_id: current.id,
                expected_revision: current.revision,
                member: spec,
            },
            Some(Callback::new(move |message| send_error.set(Some(message)))),
        );
    };
    let pinned = move || member.with(|member| member.as_ref().is_some_and(|member| member.pinned));
    let selection = Memo::new(move |_| {
        member.with(|member| {
            member
                .as_ref()
                .map(|member| (member.launch_profile_id.clone(), member.backend_kind))
        })
    });
    view! {
        <li class="swarm-preview-member" class:pinned=pinned>
            <Show when=move || member.with(|member| member.is_some()) fallback=|| view! { <span role="alert">"This member is no longer in the draft."</span> }>
                <span class="swarm-avatar swarm-avatar-sm" style=avatar_style(&member_id.get_value().0) aria-hidden="true">
                    {move || member.with(|member| member.as_ref().map(|member| initials(&member.name)))}
                </span>
                <div class="swarm-preview-member-fields">
                    <div class="swarm-preview-member-line">
                        <input
                            class="swarm-input swarm-input-compact"
                            type="text"
                            aria-label="Member name"
                            prop:value=move || name.get().unwrap_or_default()
                            on:input=move |ev| name_edit.set(Some(event_target_value(&ev)))
                        />
                        <span class="swarm-preview-backend">{move || selection.get().map(|(id, backend)| format!("{} · {}", backend_name(backend), id.0))}</span>
                    </div>
                    <input
                        class="swarm-input swarm-input-compact"
                        type="text"
                        aria-label="Member focus"
                        placeholder="Focus (optional), e.g. tests and CI"
                        prop:value=move || focus.get().unwrap_or_default()
                        on:input=move |ev| focus_edit.set(Some(event_target_value(&ev)))
                    />
                    <button
                        class="swarm-link-btn"
                        aria-expanded=move || settings_open.get().to_string()
                        on:click=move |_| settings_open.update(|open| *open = !*open)
                    >
                        {move || if settings_open.get() { "Hide model settings" } else { "Model settings" }}
                    </button>
                    <Show when=move || settings_open.get()>
                        {move || selection.get().map(|(profile_id, backend)| view! {
                            <SettingsOverrides
                                host=host
                                profile_id=profile_id
                                backend=backend
                                values=Signal::derive(move || settings.get().unwrap_or_default())
                                on_change=Callback::new(move |values| settings_edit.set(Some(values)))
                            />
                        })}
                    </Show>
                    {move || send_error.get().map(|message| view! { <span class="swarm-composer-error" role="alert">{format!("Could not save member: {message}")}</span> })}
                </div>
                <div class="swarm-preview-member-actions">
                    <Show when=move || dirty.get()>
                        <button class="swarm-btn swarm-btn-primary" title="Saving pins this member" on:click=move |_| edit(true)>
                            "Save"
                        </button>
                    </Show>
                    <button
                        class="swarm-btn"
                        class:active=pinned
                        aria-pressed=move || pinned().to_string()
                        title=move || if pinned() { "Pinned members are kept when the preview is regenerated" } else { "Keep this member when regenerating" }
                        on:click=move |_| edit(!pinned())
                    >
                        {move || if pinned() { "Pinned" } else { "Pin" }}
                    </button>
                </div>
            </Show>
        </li>
    }
}

// ── Live changes ───────────────────────────────────────────────────────────

#[component]
pub fn ManageSwarmDialog(
    host_id: String,
    swarm_id: SwarmId,
    on_close: Callback<()>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let host_streams = state.host_streams;
    let swarms_signal = state.swarms;
    let errors_signal = state.swarm_errors;
    let host = StoredValue::new(host_id);
    let sid = StoredValue::new(swarm_id);
    let form = ConstraintsForm::new();
    let retirement = RwSignal::new(SwarmRetirementPolicy::FinishTurn);
    let form_error: RwSignal<Option<String>> = RwSignal::new(None);
    // The preview revision on screen when PreviewChange was sent.
    let previewing: RwSignal<Option<Option<u64>>> = RwSignal::new(None);

    let swarm = Memo::new(move |_| {
        swarms_signal.with(|map| {
            map.get(&host.get_value())
                .and_then(|m| m.get(&sid.get_value()).cloned())
        })
    });
    let errors = Memo::new(move |_| {
        errors_signal.with(|errors| {
            errors_for(errors, &host.get_value(), |entry| {
                entry.error.swarm_id.as_ref() == Some(&sid.get_value())
                    && entry.error.publication_id.is_none()
            })
        })
    });
    Effect::new(move |_| {
        let Some(sent_from) = previewing.get() else {
            return;
        };
        let revision = swarm.with(|swarm| {
            swarm.as_ref().and_then(|swarm| {
                swarm
                    .change_preview
                    .as_ref()
                    .map(|preview| preview.revision)
            })
        });
        if revision != sent_from || !errors.with(|errors| errors.is_empty()) {
            previewing.set(None);
        }
    });
    let seeded = RwSignal::new(false);
    Effect::new(move |_| {
        if seeded.get_untracked() {
            return;
        }
        if let Some(current) = swarm.get() {
            // Continue from a pending preview so it can be adjusted.
            let constraints = current
                .change_preview
                .as_ref()
                .map(|preview| preview.constraints.clone())
                .unwrap_or_else(|| current.constraints.clone());
            form.load(&constraints);
            seeded.set(true);
        }
    });

    let form_changed = Memo::new(move |_| {
        form.track_constraints();
        swarm.with(|swarm| {
            swarm
                .as_ref()
                .and_then(|swarm| swarm.change_preview.as_ref())
                .is_none_or(|preview| {
                    form.constraints().ok().as_ref() != Some(&preview.constraints)
                })
        })
    });

    let on_send_error = Callback::new(move |message: String| {
        previewing.set(None);
        form_error.set(Some(format!("Could not reach the host: {message}")));
    });
    let send = move |command: SwarmCommandPayload| {
        form_error.set(None);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            command,
            Some(on_send_error),
        );
    };

    let preview_change = move |_| {
        let Some(current) = swarm.get_untracked() else {
            return;
        };
        match form.constraints() {
            Ok(constraints) => {
                previewing.set(Some(
                    current
                        .change_preview
                        .as_ref()
                        .map(|preview| preview.revision),
                ));
                send(SwarmCommandPayload::PreviewChange {
                    swarm_id: current.id,
                    expected_revision: current.revision,
                    constraints,
                });
            }
            Err(message) => form_error.set(Some(message)),
        }
    };
    let discard_preview = move |_| {
        if let Some(current) = swarm.get_untracked() {
            form.load(&current.constraints);
        }
        send(SwarmCommandPayload::DiscardChangePreview {
            swarm_id: sid.get_value(),
        });
    };

    let member_label = move |id: &protocol::SwarmMemberId| {
        swarm.with_untracked(|swarm| {
            swarm
                .as_ref()
                .and_then(|swarm| swarm.members.iter().find(|member| &member.spec.id == id))
                .map(|member| format!("{} · {}", member.spec.name, member_status_label(member)))
                .unwrap_or_else(|| id.0.clone())
        })
    };

    let preview_view = move || {
        let Some(current) = swarm.get() else {
            return view! { <div class="swarm-empty">"This swarm is no longer available."</div> }
                .into_any();
        };
        let Some(preview) = current.change_preview.clone() else {
            return view! {
                <div class="swarm-empty">
                    "Adjust the constraints and preview the change. Members only change when you apply a preview."
                </div>
            }
            .into_any();
        };
        let stale = preview.base_revision != current.revision;
        let applicable = !stale && preview.conflicts.is_empty();
        let preview_revision = preview.revision;
        let retained = preview
            .retained
            .iter()
            .map(member_label)
            .collect::<Vec<_>>();
        let retirements = preview
            .retirements
            .iter()
            .map(member_label)
            .collect::<Vec<_>>();
        let additions = preview
            .additions
            .iter()
            .map(|spec| format!("{} · {}", spec.name, backend_name(spec.backend_kind)))
            .collect::<Vec<_>>();
        let has_retirements = !retirements.is_empty();
        let list = |title: &'static str, tone: &'static str, items: Vec<String>| {
            (!items.is_empty()).then(|| view! {
                <div class="swarm-change-group" data-tone=tone>
                    <h4 class="swarm-change-title">{format!("{title} ({})", items.len())}</h4>
                    <ul>{items.into_iter().map(|item| view! { <li>{item}</li> }).collect_view()}</ul>
                </div>
            })
        };
        view! {
            <div class="swarm-change-preview">
                <p class="swarm-field-help">{format!("Preview revision {preview_revision}")}</p>
                <SwarmToolPolicyNote />
                {stale.then(|| view! {
                    <div class="swarm-banner" data-tone="warn" role="status">
                        <span class="swarm-banner-text">"The swarm changed since this preview. Preview again before applying."</span>
                    </div>
                })}
                <Show when=move || form_changed.get()>
                    <div class="swarm-banner" data-tone="warn" role="status">
                        <span class="swarm-banner-text">"Your edits are not in this preview. Preview again before applying."</span>
                    </div>
                </Show>
                {(!preview.conflicts.is_empty()).then(|| view! {
                    <div class="swarm-banner swarm-banner-list" data-tone="warn" role="status">
                        <ul>{preview.conflicts.iter().cloned().map(|conflict| view! { <li>{conflict}</li> }).collect_view()}</ul>
                    </div>
                })}
                {list("Keep", "ok", retained)}
                {list("Add", "info", additions)}
                {list("Retire", "warn", retirements)}
                {has_retirements.then(|| view! {
                    <fieldset class="swarm-field swarm-policy">
                        <legend class="swarm-field-label">"Retiring members"</legend>
                        <label class="swarm-radio">
                            <input
                                type="radio"
                                name="swarm-retirement"
                                prop:checked=move || retirement.get() == SwarmRetirementPolicy::FinishTurn
                                on:change=move |_| retirement.set(SwarmRetirementPolicy::FinishTurn)
                            />
                            <span class="swarm-radio-title">"Let them finish their current turn"</span>
                        </label>
                        <label class="swarm-radio">
                            <input
                                type="radio"
                                name="swarm-retirement"
                                prop:checked=move || retirement.get() == SwarmRetirementPolicy::InterruptNow
                                on:change=move |_| retirement.set(SwarmRetirementPolicy::InterruptNow)
                            />
                            <span class="swarm-radio-title">"Interrupt them now"</span>
                        </label>
                    </fieldset>
                })}
                <div class="swarm-change-actions">
                    <button class="swarm-btn swarm-btn-quiet" on:click=discard_preview>"Discard preview"</button>
                    <button
                        class="swarm-btn swarm-btn-primary"
                        data-action="apply"
                        disabled=move || !applicable || form_changed.get() || previewing.get().is_some()
                        on:click=move |_| {
                            if !applicable || form_changed.get_untracked() || previewing.get_untracked().is_some() {
                                return;
                            }
                            send(SwarmCommandPayload::ApplyChange {
                                swarm_id: sid.get_value(),
                                preview_revision,
                                retirement: retirement.get_untracked(),
                            });
                            on_close.run(());
                        }
                    >
                        {format!("Apply preview {preview_revision}")}
                    </button>
                </div>
            </div>
        }
        .into_any()
    };

    view! {
        <SwarmModal on_close=on_close labelled_by="swarm-manage-title">
            <header class="swarm-modal-header">
                <h2 id="swarm-manage-title" class="swarm-modal-title">
                    {move || swarm.with(|s| s.as_ref().map(|s| format!("Manage {}", s.name)).unwrap_or_else(|| "Manage swarm".to_owned()))}
                </h2>
                <p class="swarm-modal-subtitle">
                    "Nothing about the running swarm changes until you apply a preview. Project and workspace access are fixed for a running swarm."
                </p>
            </header>
            <div class="swarm-modal-body swarm-modal-split">
                <section class="swarm-modal-pane" aria-label="Constraints">
                    <ConstraintsFields host=host form=form locked_scope=true />
                </section>
                <section
                    class="swarm-modal-pane swarm-preview-pane"
                    aria-label="Change preview"
                    aria-busy=move || previewing.get().is_some().to_string()
                >
                    <h3 class="swarm-pane-title">"Change preview"</h3>
                    {move || previewing.get().map(|_| view! {
                        <div class="swarm-loading" role="status">"Previewing change…"</div>
                    })}
                    {preview_view}
                </section>
            </div>
            <div class="swarm-modal-errors">
                {move || form_error.get().map(|message| view! {
                    <div class="swarm-banner" data-tone="error" role="alert"><span class="swarm-banner-text">{message}</span></div>
                })}
                <ErrorList errors=Signal::derive(move || errors.get()) />
            </div>
            <footer class="swarm-modal-footer">
                <span class="swarm-footer-note"></span>
                <button class="swarm-btn swarm-btn-quiet" on:click=move |_| on_close.run(())>"Close"</button>
                <button
                    class="swarm-btn swarm-btn-primary"
                    data-action="preview-change"
                    disabled=move || previewing.get().is_some() || form.numeric_error().is_some()
                    on:click=preview_change
                >
                    "Preview change"
                </button>
            </footer>
        </SwarmModal>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;

    use crate::components::center_zone::CenterZone;
    use crate::components::project_rail::ProjectRail;
    use crate::components::swarm_view::wasm_tests::{
        Harness, PROJECT, all, button, ensure_styles_loaded, has_button, idle, make_container,
        make_swarm, member, mount_view, one, press, press_with, settle, spec, text_of, type_into,
    };
    use crate::components::swarms_panel::SwarmsPanel;
    use crate::state::{ActiveProjectRef, TabContent};
    use leptos::mount::mount_to;
    use protocol::{
        FrameKind, LaunchProfileCatalog, LaunchProfileKind, Project, ProjectSource,
        SwarmChangePreview, SwarmDraftGeneration, SwarmDraftNotifyPayload, SwarmMemberId,
        SwarmMemberState,
    };
    use serde_json::json;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn active_element() -> Option<web_sys::Element> {
        web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .active_element()
    }

    fn is_disabled(button: &HtmlElement) -> bool {
        button
            .dyn_ref::<web_sys::HtmlButtonElement>()
            .expect("button")
            .disabled()
    }

    fn modal(root: &web_sys::Element) -> Option<web_sys::Element> {
        // A prior WASM panic can leave its mount behind in the shared document.
        // Inspect this fixture, not the first dialog belonging to another test.
        let selector = ".swarm-modal[role='dialog']";
        let dialogs = root.query_selector_all(selector).unwrap();
        let document_dialogs = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .query_selector_all(selector)
            .unwrap();
        console_log!(
            "swarm modal lookup: fixture_dialogs={}, document_dialogs={}",
            dialogs.length(),
            document_dialogs.length()
        );
        assert!(dialogs.length() <= 1, "one modal per fixture");
        root.query_selector(selector).unwrap()
    }

    /// A host with one project and a ready default launch profile, published
    /// after the bootstrap so they are the host's current catalog.
    fn host_with_catalog(host: &str) -> Harness {
        let harness = Harness::new(host);
        harness.state.projects.set(vec![ProjectInfo {
            host_id: host.to_owned(),
            project: Project {
                id: ProjectId(PROJECT.to_owned()),
                name: "Checkout".to_owned(),
                sort_order: 0,
                source: ProjectSource::Standalone { roots: Vec::new() },
            },
        }]);
        harness.state.active_project.set(Some(ActiveProjectRef {
            host_id: host.to_owned(),
            project_id: ProjectId(PROJECT.to_owned()),
        }));
        harness.state.launch_profile_catalog.update(|catalogs| {
            catalogs.insert(
                host.to_owned(),
                LaunchProfileCatalog {
                    entries: vec![LaunchProfileEntry::Ready {
                        profile: LaunchProfile {
                            id: LaunchProfileId("claude-default".to_owned()),
                            kind: LaunchProfileKind::BackendDefault,
                            label: "Default".to_owned(),
                            description: None,
                            backend_kind: BackendKind::Claude,
                            session_settings: SessionSettingsValues::default(),
                        },
                    }],
                    default_profile_id: Some(LaunchProfileId("claude-default".to_owned())),
                    ..LaunchProfileCatalog::default()
                },
            );
        });
        harness
    }

    fn mount_panel(harness: &Harness) -> (HtmlElement, impl Sized) {
        ensure_styles_loaded();
        let container = make_container();
        let state = harness.state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <SwarmsPanel /> <CenterZone /> }
        });
        (container, handle)
    }

    fn field(root: &web_sys::Element, name: &str) -> HtmlElement {
        one(root, &format!("[data-field='{name}']"))
    }

    fn action(root: &web_sys::Element, name: &str) -> HtmlElement {
        one(root, &format!("[data-action='{name}']"))
    }

    fn change_control(control: &HtmlElement, value: &str) {
        if let Some(select) = control.dyn_ref::<web_sys::HtmlSelectElement>() {
            select.set_value(value);
        } else {
            control
                .dyn_ref::<web_sys::HtmlInputElement>()
                .expect("input")
                .set_value(value);
        }
        control
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
    }

    async fn assert_edited_preview_blocked(harness: &Harness, dialog: &web_sys::Element) {
        settle().await;
        let apply = action(dialog, "apply");
        assert!(
            is_disabled(&apply),
            "every edited constraint requires a matching preview"
        );
        assert!(
            text_of(dialog)
                .contains("Your edits are not in this preview. Preview again before applying.")
        );
        apply.click();
        settle().await;
        assert!(
            harness.commands_of("apply_change").is_empty(),
            "edited constraints cannot apply the old preview"
        );
    }

    #[wasm_bindgen_test]
    async fn writable_scope_choices_authorize_exact_scope_with_host_acknowledgement() {
        for (key, expected, scope_text, selected) in [
            (
                "shared_host",
                SwarmWorkspacePolicy::SharedHost {
                    writable_consent: true,
                },
                "all projects and workbenches on this host",
                PROJECT,
            ),
            (
                "shared_project",
                SwarmWorkspacePolicy::SharedProject {
                    writable_consent: true,
                },
                // The redundant acceptance label is gone; the scope help
                // still states the complete project and future-workbench grant.
                "Members can edit the selected project and all its workbenches, including newly created workbenches.",
                PROJECT,
            ),
            (
                "shared_workbench",
                SwarmWorkspacePolicy::SharedWorkbench {
                    writable_consent: true,
                },
                "Members can edit this workbench. Nothing is landed on main automatically.",
                "scope-workbench",
            ),
        ] {
            let harness = host_with_catalog("host-swarm-scopes");
            let (container, handle) = mount_panel(&harness);
            settle().await;
            button(&container, "+ New swarm").click();
            settle().await;
            let dialog = modal(&container).expect("scope dialog");
            let content = text_of(&dialog);
            assert!(content.contains("Host scope — writable"));
            assert!(content.contains("Project scope — writable"));
            assert!(content.contains("Workbench scope — writable"));
            assert!(content.contains("Read-only project access"));
            assert!(
                one(&dialog, "[data-policy='shared_workbench']")
                    .dyn_ref::<web_sys::HtmlInputElement>()
                    .expect("workspace scope is a radio input")
                    .disabled()
            );
            harness.emit(
                FrameKind::ProjectNotify,
                &protocol::ProjectNotifyPayload::Upsert {
                    project: Project {
                        id: ProjectId("scope-workbench".to_owned()),
                        name: "Scope workbench".to_owned(),
                        sort_order: 1,
                        source: ProjectSource::GitWorkbench {
                            parent_project_id: ProjectId(PROJECT.to_owned()),
                            branch: protocol::GitBranchName("scope-workbench".to_owned()),
                            roots: Vec::new(),
                        },
                    },
                },
            );
            settle().await;
            change_control(&field(&dialog, "project"), "scope-workbench");
            settle().await;
            one(&dialog, &format!("[data-policy='{key}']")).click();
            settle().await;
            if key == "shared_host" {
                change_control(&field(&dialog, "project"), PROJECT);
                settle().await;
                assert!(text_of(&dialog).contains("Starting project"));
            }
            assert_eq!(
                field(&dialog, "project")
                    .dyn_ref::<web_sys::HtmlSelectElement>()
                    .expect("project selector")
                    .value(),
                selected
            );
            assert!(text_of(&dialog).contains(scope_text));
            type_into(&field(&dialog, "name"), "Scoped swarm");
            if key == "shared_host" {
                action(&dialog, "generate").click();
                settle().await;
                assert!(
                    harness.commands_of("generate_draft").is_empty(),
                    "host-wide writes still need separate acknowledgement"
                );
                assert!(text_of(&dialog).contains("Confirm write access to the selected scope."));
                field(&dialog, "writable-consent").click();
                settle().await;
            } else {
                // Selecting the explicit project/workbench write scope is the
                // authorization; a second checkbox is no longer required.
                assert!(
                    dialog
                        .query_selector("[data-field='writable-consent']")
                        .unwrap()
                        .is_none()
                );
                assert!(!text_of(&dialog).contains("Edits are not serialized"));
            }
            action(&dialog, "generate").click();
            settle().await;
            let commands = harness.commands_of("generate_draft");
            assert_eq!(commands.len(), 1);
            let constraints: SwarmConstraints =
                serde_json::from_value(commands[0]["constraints"].clone())
                    .expect("typed scope constraints");
            assert_eq!(constraints.workspace_policy, expected);
            assert_eq!(constraints.project_id, ProjectId(selected.to_owned()));
            drop(handle);
            container.remove();
        }
    }

    /// Constraints first, then the host's preview, then launch of exactly the
    /// previewed revision; the launched swarm opens as a tab and focus
    /// returns to where the user started.
    #[wasm_bindgen_test]
    async fn new_swarm_previews_on_host_and_launches_the_reviewed_revision() {
        let harness = host_with_catalog("host-swarm-create");
        let settings_host = "host-settings-local";
        harness
            .state
            .selected_host_id
            .set(Some(settings_host.to_owned()));
        harness.state.active_project.set(None);
        harness.state.configured_hosts.set(vec![
            crate::bridge::ConfiguredHost {
                id: settings_host.to_owned(),
                label: "Local host".to_owned(),
                transport: crate::bridge::HostTransportConfig::LocalEmbedded,
                auto_connect: true,
            },
            crate::bridge::ConfiguredHost {
                id: harness.host.clone(),
                label: "Remote host".to_owned(),
                transport: crate::bridge::HostTransportConfig::SshStdio {
                    ssh_destination: "remote-test".to_owned(),
                    lifecycle: crate::bridge::RemoteHostLifecycleConfig::Manual,
                },
                auto_connect: true,
            },
        ]);
        harness.state.projects.update(|projects| {
            projects.push(ProjectInfo {
                host_id: settings_host.to_owned(),
                project: Project {
                    id: ProjectId("local-checkout".to_owned()),
                    name: "Local checkout".to_owned(),
                    sort_order: 0,
                    source: ProjectSource::Standalone { roots: Vec::new() },
                },
            })
        });
        harness.state.swarms.update(|hosts| {
            let local = make_swarm("local-swarm", "Local settings swarm", Vec::new());
            hosts.insert(
                settings_host.to_owned(),
                HashMap::from([(local.id.clone(), local)]),
            );
        });
        harness.swarm(&make_swarm(
            "remote-swarm",
            "Remote project swarm",
            Vec::new(),
        ));
        harness.emit(
            FrameKind::SessionSchemas,
            &protocol::SessionSchemasPayload {
                schemas: vec![SessionSchemaEntry::Ready {
                    schema: protocol::SessionSettingsSchema {
                        backend_kind: BackendKind::Claude,
                        fields: vec![protocol::SessionSettingField {
                            key: "model".to_owned(),
                            label: "Model".to_owned(),
                            description: None,
                            field_type: protocol::SessionSettingFieldType::Select {
                                options: vec![protocol::SelectOption {
                                    value: "alternate-model".to_owned(),
                                    label: "Alternate model".to_owned(),
                                }],
                                default: None,
                                nullable: true,
                            },
                            use_slider: false,
                            select_options_by_setting: None,
                        }],
                        model_resolutions: Default::default(),
                    },
                }],
            },
        );
        ensure_styles_loaded();
        let container = make_container();
        let state = harness.state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <ProjectRail /> <SwarmsPanel /> <CenterZone /> }
        });
        settle().await;
        assert!(text_of(&container).contains("Local settings swarm"));
        one(&container, ".rail-project-row button[title='Checkout']").click();
        settle().await;
        console_log!(
            "swarm host routing: project_matches_settings={}, remote_card_visible={}",
            harness.state.active_project.get_untracked().is_some_and(
                |active| Some(active.host_id) == harness.state.selected_host_id.get_untracked()
            ),
            text_of(&container).contains("Remote project swarm")
        );
        assert!(
            text_of(&container).contains("Remote project swarm"),
            "the swarm list follows the selected remote project, not the Settings host"
        );
        assert!(!text_of(&container).contains("Local settings swarm"));

        let opener = button(&container, "+ New swarm");
        opener.focus().unwrap();
        opener.click();
        settle().await;
        let dialog = modal(&container).expect("dialog open");
        assert_eq!(text_of(&one(&dialog, ".swarm-modal-title")), "New swarm");
        let name = field(&dialog, "name");
        assert_eq!(
            active_element().as_ref(),
            Some(name.unchecked_ref::<web_sys::Element>()),
            "focus starts in the form"
        );
        assert_eq!(
            field(&dialog, "project")
                .dyn_ref::<web_sys::HtmlSelectElement>()
                .map(|select| select.value()),
            Some(PROJECT.to_owned()),
            "the active project is preselected"
        );
        let launch = action(&dialog, "launch");
        assert!(is_disabled(&launch));
        assert_eq!(
            launch.get_attribute("title").as_deref(),
            Some("Generate a preview first")
        );

        type_into(&name, "Checkout reliability");
        for field_name in ["brief", "guidance", "wake-budget"] {
            assert!(
                dialog
                    .query_selector(&format!("[data-field='{field_name}']"))
                    .unwrap()
                    .is_none(),
                "creation contains only essential setup"
            );
        }
        action(&dialog, "generate").click();
        settle().await;

        let generated = harness.commands_of("generate_draft");
        assert_eq!(generated.len(), 1);
        let command = &generated[0];
        assert_eq!(command["expected_revision"], serde_json::Value::Null);
        assert_eq!(command["name"], "Checkout reliability");
        assert_eq!(
            command.get("opening_brief"),
            None,
            "the first message belongs in the conversation, not setup"
        );
        let constraints: SwarmConstraints =
            serde_json::from_value(command["constraints"].clone()).unwrap();
        assert_eq!(constraints.project_id, ProjectId(PROJECT.to_owned()));
        assert_eq!(constraints.workspace_policy, SwarmWorkspacePolicy::ReadOnly);
        assert_eq!(constraints.max_live_agents, 4);
        assert_eq!(
            constraints.agent_wake_budget, None,
            "agents wake each other without limit unless a limit is set"
        );
        assert_eq!(constraints.allocations.len(), 1);
        assert_eq!(
            constraints.allocations[0].launch_profile_id,
            LaunchProfileId("claude-default".to_owned())
        );
        assert_eq!(constraints.allocations[0].count, 2);
        assert!(text_of(&dialog).contains("Generating preview…"));
        assert!(
            is_disabled(&action(&dialog, "launch")),
            "nothing launches while the preview is pending"
        );

        let draft_id = SwarmDraftId(command["draft_id"].as_str().unwrap().to_owned());
        let mut members = vec![spec("m1", "Generalist 1"), spec("m2", "Generalist 2")];
        members[1].focus = Some("tests".to_owned());
        let mut reviewed_draft = SwarmDraft {
            legacy_source: None,
            retained_sessions: Default::default(),
            id: draft_id.clone(),
            revision: 1,
            name: "Checkout reliability".to_owned(),
            constraints: constraints.clone(),
            members,
            conflicts: Vec::new(),
            generation: SwarmDraftGeneration::DeterministicGeneralists,
            legacy_team_id: None,
        };
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(reviewed_draft.clone()),
            },
        );
        settle().await;
        let dialog = modal(&container).expect("dialog still open");
        assert!(!text_of(&dialog).contains("Generating preview…"));
        assert_eq!(all(&dialog, ".swarm-preview-member").len(), 2);
        assert!(
            text_of(&field(&dialog, "tool-policy"))
                .contains("Your other configured MCP servers are not attached"),
            "the tool restriction is shown before launch"
        );
        let launch = action(&dialog, "launch");
        assert_eq!(text_of(&launch), "Create swarm");
        assert!(!is_disabled(&launch));
        for (control, invalid, original, message) in [
            (
                field(&dialog, "max-live"),
                "",
                "4",
                "Live agents must be a whole number from 1 to 16.",
            ),
            (
                one(&dialog, "[aria-label='Agents from this profile']"),
                "1.5",
                "2",
                "Agents from this profile must be a whole number from 1 to 16.",
            ),
        ] {
            change_control(&control, invalid);
            settle().await;
            assert!(is_disabled(&action(&dialog, "launch")));
            assert!(is_disabled(&action(&dialog, "generate")));
            assert!(text_of(&dialog).contains(message));
            assert_eq!(
                control.get_attribute("aria-invalid").as_deref(),
                Some("true")
            );
            action(&dialog, "launch").click();
            action(&dialog, "generate").click();
            settle().await;
            assert!(harness.commands_of("launch").is_empty());
            assert_eq!(harness.commands_of("generate_draft").len(), 1);
            change_control(&control, original);
            settle().await;
            assert!(!is_disabled(&action(&dialog, "launch")));
            assert!(!is_disabled(&action(&dialog, "generate")));
        }

        type_into(&field(&dialog, "name"), "Checkout reliability v2");
        settle().await;
        assert!(
            is_disabled(&action(&dialog, "launch")),
            "an edited form is not what the preview shows"
        );
        assert_eq!(
            action(&dialog, "launch").get_attribute("title").as_deref(),
            Some("Update the preview so you launch what you see")
        );
        type_into(&field(&dialog, "name"), "Checkout reliability");
        settle().await;
        let launch = action(&dialog, "launch");
        assert!(!is_disabled(&launch));

        let rows = all(&dialog, ".swarm-preview-member");
        let first_name = one(&rows[0], "[aria-label='Member name']");
        let first_focus = one(&rows[0], "[aria-label='Member focus']");
        let second_name = one(&rows[1], "[aria-label='Member name']");
        let second_focus = one(&rows[1], "[aria-label='Member focus']");
        type_into(&first_name, "Checkout guide");
        type_into(&first_focus, "retry behavior");
        type_into(&second_name, "Test guide");
        type_into(&second_focus, "retry regression coverage");
        button(&rows[1], "Model settings").click();
        settle().await;
        let second_model = one(&rows[1], ".session-setting-select");
        change_control(&second_model, "alternate-model");
        settle().await;
        assert!(
            is_disabled(&launch),
            "visible member edits cannot launch the previous saved selection"
        );
        assert_eq!(
            launch.get_attribute("title").as_deref(),
            Some("Save member edits before launching")
        );
        assert!(text_of(&dialog).contains("Some member edits are not saved on the host."));
        // Leptos skips delegated events on disabled controls; exercise a stale
        // enabled DOM as well so only the handler's own guard can block launch.
        let launch_button = launch.dyn_ref::<web_sys::HtmlButtonElement>().unwrap();
        launch_button.set_disabled(false);
        launch.click();
        launch_button.set_disabled(true);
        settle().await;
        assert!(
            harness.commands_of("launch").is_empty(),
            "the launch handler also rejects unsaved rows"
        );

        button(&rows[0], "Save").click();
        settle().await;
        let edits = harness.commands_of("edit_draft_member");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["expected_revision"], 1);
        let saved_first: SwarmMemberSpec =
            serde_json::from_value(edits[0]["member"].clone()).unwrap();
        assert!(saved_first.id == reviewed_draft.members[0].id);
        assert!(saved_first.name == "Checkout guide");
        assert!(saved_first.focus.as_deref() == Some("retry behavior"));
        assert!(saved_first.pinned);
        second_name.focus().unwrap();
        let second_name_input = second_name.dyn_ref::<web_sys::HtmlInputElement>().unwrap();
        second_name_input.set_selection_range(2, 6).unwrap();
        reviewed_draft.members[0] = saved_first;
        reviewed_draft.revision = 2;
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(reviewed_draft.clone()),
            },
        );
        settle().await;
        let current_rows = all(&dialog, ".swarm-preview-member");
        console_log!(
            "draft revision reconciliation: member_rows={}, edited_member_focused={}, launch_blocked={}",
            current_rows.len(),
            active_element().is_some_and(|active| active.is_same_node(Some(&second_name))),
            is_disabled(&launch)
        );
        assert_eq!(current_rows.len(), 2);
        for (original, selector) in [
            (&second_name, "[aria-label='Member name']"),
            (&second_focus, "[aria-label='Member focus']"),
            (&second_model, ".session-setting-select"),
        ] {
            assert!(
                original.is_same_node(Some(&one(&current_rows[1], selector))),
                "saving another member must not remount this editor"
            );
        }
        assert!(second_name_input.value() == "Test guide");
        assert!(
            second_focus
                .dyn_ref::<web_sys::HtmlInputElement>()
                .unwrap()
                .value()
                == "retry regression coverage"
        );
        assert!(
            second_model
                .dyn_ref::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value()
                == "alternate-model"
        );
        assert!(active_element().is_some_and(|active| active.is_same_node(Some(&second_name))));
        assert_eq!(second_name_input.selection_start().unwrap(), Some(2));
        assert_eq!(second_name_input.selection_end().unwrap(), Some(6));
        assert!(!has_button(&current_rows[0], "Save"));
        assert!(has_button(&current_rows[0], "Pinned"));
        assert!(has_button(&current_rows[1], "Save"));
        assert!(has_button(&current_rows[1], "Hide model settings"));
        assert!(is_disabled(&launch));

        let mut unrelated_draft = reviewed_draft.clone();
        unrelated_draft.id = SwarmDraftId("unrelated-draft".to_owned());
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(unrelated_draft),
            },
        );
        settle().await;
        assert!(second_name_input.value() == "Test guide");
        assert!(active_element().is_some_and(|active| active.is_same_node(Some(&second_name))));
        assert!(is_disabled(&launch));

        reviewed_draft.members[0].focus = Some("canonical updated focus".to_owned());
        reviewed_draft.revision = 3;
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(reviewed_draft.clone()),
            },
        );
        settle().await;
        assert!(
            first_focus
                .dyn_ref::<web_sys::HtmlInputElement>()
                .unwrap()
                .value()
                == "canonical updated focus",
            "acknowledged overrides clear so clean fields follow later canonical revisions"
        );
        assert!(second_name_input.value() == "Test guide");
        assert!(
            second_focus
                .dyn_ref::<web_sys::HtmlInputElement>()
                .unwrap()
                .value()
                == "retry regression coverage"
        );
        assert!(
            second_model
                .dyn_ref::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value()
                == "alternate-model"
        );
        assert!(active_element().is_some_and(|active| active.is_same_node(Some(&second_name))));
        assert_eq!(second_name_input.selection_start().unwrap(), Some(2));
        assert_eq!(second_name_input.selection_end().unwrap(), Some(6));
        assert!(is_disabled(&launch));

        button(&rows[1], "Save").click();
        settle().await;
        let edits = harness.commands_of("edit_draft_member");
        assert_eq!(edits.len(), 2);
        assert_eq!(
            edits[1]["expected_revision"], 3,
            "stable editors save against the latest canonical revision, not their mount revision"
        );
        let saved_second: SwarmMemberSpec =
            serde_json::from_value(edits[1]["member"].clone()).unwrap();
        assert!(saved_second.id == SwarmMemberId("m2".to_owned()));
        assert!(saved_second.name == "Test guide");
        assert!(saved_second.focus.as_deref() == Some("retry regression coverage"));
        assert!(
            saved_second.session_settings.0.get("model")
                == Some(&protocol::SessionSettingValue::String(
                    "alternate-model".to_owned()
                ))
        );
        assert!(saved_second.pinned);
        assert!(
            is_disabled(&launch),
            "sending Save does not approve a selection before the canonical event"
        );
        let canonical_second = reviewed_draft
            .members
            .iter_mut()
            .find(|member| member.id == saved_second.id)
            .unwrap();
        *canonical_second = saved_second;
        reviewed_draft.revision = 4;
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(reviewed_draft.clone()),
            },
        );
        settle().await;
        assert!(!has_button(&rows[1], "Save"));
        assert!(has_button(&rows[1], "Pinned"));
        assert!(!is_disabled(&launch));
        assert_eq!(text_of(&launch), "Create swarm");

        launch.focus().unwrap();
        reviewed_draft.members.swap(0, 1);
        reviewed_draft.revision = 5;
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(reviewed_draft),
            },
        );
        settle().await;
        let reordered_rows = all(&dialog, ".swarm-preview-member");
        assert!(
            rows[1].is_same_node(Some(&reordered_rows[0]))
                && rows[0].is_same_node(Some(&reordered_rows[1])),
            "canonical reordering must keep each editor with its member ID"
        );
        assert!(second_name_input.value() == "Test guide");
        assert!(
            second_model
                .dyn_ref::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value()
                == "alternate-model"
        );
        assert!(!is_disabled(&launch));
        assert_eq!(text_of(&launch), "Create swarm");
        launch.click();
        settle().await;
        assert_eq!(
            harness.commands_of("launch"),
            vec![json!({"kind": "launch", "draft_id": draft_id.0, "expected_revision": 5})]
        );
        assert!(modal(&container).is_none(), "launch closes the dialog");
        assert_eq!(
            active_element().as_ref(),
            Some(opener.unchecked_ref::<web_sys::Element>())
        );

        let mut launched = make_swarm(
            "sw-created",
            "Checkout reliability",
            vec![idle("m1", "Generalist 1")],
        );
        launched.source_draft_id = Some(draft_id);
        harness.swarm(&launched);
        settle().await;
        assert_eq!(
            harness
                .state
                .center_zone
                .with_untracked(|cz| cz.active_content().cloned()),
            Some(TabContent::Swarm {
                host_id: harness.host.clone(),
                swarm_id: SwarmId("sw-created".into())
            }),
            "the launched swarm's boards open"
        );
        assert_eq!(
            text_of(&one(&container, ".swarm-view .swarm-title")),
            "Checkout reliability"
        );
        assert!(
            harness
                .command_hosts()
                .iter()
                .all(|host| host == &harness.host),
            "every preview, member edit, and launch command targets the remote project host"
        );
        assert!(
            harness.state.selected_host_id.get_untracked().as_deref() == Some(settings_host),
            "project navigation never changes the Settings host"
        );

        harness.state.selected_host_id.set(None);
        settle().await;
        assert!(
            !is_disabled(&opener),
            "a project's host is sufficient to create a swarm"
        );
        opener.click();
        settle().await;
        let dialog = modal(&container).expect("remote dialog without a Settings host");
        assert_eq!(
            field(&dialog, "project")
                .dyn_ref::<web_sys::HtmlSelectElement>()
                .unwrap()
                .value(),
            PROJECT
        );
        press(dialog.unchecked_ref(), "Escape");
        settle().await;
        harness
            .state
            .selected_host_id
            .set(Some(settings_host.to_owned()));
        one(
            &container,
            ".rail-project-row button[title='Local checkout']",
        )
        .click();
        settle().await;
        assert!(text_of(&container).contains("Local settings swarm"));
        assert!(!text_of(&container).contains("Remote project swarm"));
        opener.click();
        settle().await;
        let dialog = modal(&container).expect("local project dialog");
        assert!(text_of(&field(&dialog, "project")).contains("Local checkout"));
        assert!(
            all(&field(&dialog, "project"), "option")
                .iter()
                .all(|option| text_of(option) != "Checkout")
        );
        press(dialog.unchecked_ref(), "Escape");
        settle().await;
        button(&container, "Home").click();
        settle().await;
        assert!(text_of(&container).contains("Local settings swarm"));
    }

    /// Tab and Shift+Tab stay inside the modal; Escape closes it and returns
    /// focus to the control that opened it.
    #[wasm_bindgen_test]
    async fn swarm_modal_traps_focus_and_escape_restores_it() {
        let harness = host_with_catalog("host-swarm-modal");
        let (container, _handle) = mount_panel(&harness);
        settle().await;
        let opener = button(&container, "+ New swarm");
        opener.focus().unwrap();
        opener.click();
        settle().await;

        let dialog = modal(&container).expect("dialog open");
        let first = field(&dialog, "name");
        let last = action(&dialog, "generate");
        assert!(
            is_disabled(&action(&dialog, "launch")),
            "the disabled launch button is not a tab stop"
        );
        assert_eq!(
            active_element().as_ref(),
            Some(first.unchecked_ref::<web_sys::Element>())
        );

        last.focus().unwrap();
        press(&last, "Tab");
        assert_eq!(
            active_element().as_ref(),
            Some(first.unchecked_ref::<web_sys::Element>()),
            "Tab wraps to the first control"
        );
        press_with(&first, "Tab", true);
        assert_eq!(
            active_element().as_ref(),
            Some(last.unchecked_ref::<web_sys::Element>()),
            "Shift+Tab wraps to the last control"
        );

        press(&last, "Escape");
        settle().await;
        assert!(modal(&container).is_none(), "Escape closes the dialog");
        assert_eq!(
            active_element().as_ref(),
            Some(opener.unchecked_ref::<web_sys::Element>())
        );
        assert!(harness.commands().is_empty(), "closing sends nothing");
    }

    /// Manage previews a change on the host against the swarm's revision and
    /// applies exactly that preview with the chosen retirement policy.
    #[wasm_bindgen_test]
    async fn manage_previews_and_applies_the_exact_change_revision() {
        let harness = host_with_catalog("host-swarm-manage");
        harness.emit(
            FrameKind::SessionSchemas,
            &protocol::SessionSchemasPayload {
                schemas: vec![SessionSchemaEntry::Ready {
                    schema: protocol::SessionSettingsSchema {
                        backend_kind: BackendKind::Claude,
                        fields: vec![protocol::SessionSettingField {
                            key: "model".to_owned(),
                            label: "Model".to_owned(),
                            description: None,
                            field_type: protocol::SessionSettingFieldType::Select {
                                options: vec![protocol::SelectOption {
                                    value: "alternate-model".to_owned(),
                                    label: "Alternate model".to_owned(),
                                }],
                                default: None,
                                nullable: true,
                            },
                            use_slider: false,
                            select_options_by_setting: None,
                        }],
                        model_resolutions: Default::default(),
                    },
                }],
            },
        );
        harness.state.launch_profile_catalog.update(|catalogs| {
            let catalog = catalogs.get_mut(&harness.host).unwrap();
            let LaunchProfileEntry::Ready { profile } = &catalog.entries[0] else {
                panic!("ready profile");
            };
            let mut alternate = profile.clone();
            alternate.id = LaunchProfileId("claude-alternate".to_owned());
            alternate.label = "Alternate profile".to_owned();
            catalog
                .entries
                .push(LaunchProfileEntry::Ready { profile: alternate });
        });
        let sid = "sw-manage";
        let mut swarm = make_swarm(
            sid,
            "Manage me",
            vec![
                idle("ada", "Ada"),
                member("bo", "Bo", SwarmMemberState::Live, Some("agent-bo"), None),
            ],
        );
        swarm.revision = 3;
        harness.swarm(&swarm);
        let (container, _handle) = mount_view(&harness, sid);
        settle().await;

        let manage = button(&container, "Manage");
        manage.focus().unwrap();
        manage.click();
        settle().await;
        let dialog = modal(&container).expect("manage dialog");
        let advanced = one(&dialog, ".swarm-advanced");
        assert!(!advanced.has_attribute("open"));
        one(&advanced, "summary")
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap()
            .click();
        settle().await;
        assert_eq!(
            text_of(&one(&dialog, ".swarm-modal-title")),
            "Manage Manage me"
        );
        assert!(
            field(&dialog, "project")
                .dyn_ref::<web_sys::HtmlSelectElement>()
                .unwrap()
                .disabled(),
            "a running swarm keeps its project"
        );
        let more = one(&dialog, "[aria-label='More live agents']");
        more.click();
        settle().await;
        action(&dialog, "preview-change").click();
        settle().await;
        let previews = harness.commands_of("preview_change");
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0]["expected_revision"], 3);
        let constraints: SwarmConstraints =
            serde_json::from_value(previews[0]["constraints"].clone()).unwrap();
        assert_eq!(
            constraints.max_live_agents,
            swarm.constraints.max_live_agents + 1
        );
        assert_eq!(constraints.allocations, swarm.constraints.allocations);
        assert!(text_of(&dialog).contains("Previewing change…"));

        swarm.change_preview = Some(SwarmChangePreview {
            revision: 7,
            base_revision: 3,
            constraints,
            retained: vec![SwarmMemberId("ada".into())],
            additions: vec![spec("cy", "Cy")],
            retirements: vec![SwarmMemberId("bo".into())],
            conflicts: Vec::new(),
        });
        harness.swarm(&swarm);
        settle().await;
        let dialog = modal(&container).expect("manage dialog");
        assert!(!text_of(&dialog).contains("Previewing change…"));
        let groups: Vec<String> = all(&dialog, ".swarm-change-title")
            .iter()
            .map(|title| text_of(title))
            .collect();
        assert_eq!(groups, ["Keep (1)", "Add (1)", "Retire (1)"]);
        assert!(
            text_of(&dialog).contains("Bo · Status unavailable"),
            "retiring members show server status only"
        );
        assert!(
            text_of(&field(&dialog, "tool-policy"))
                .contains("ten swarm board tools (including atomic thread state and delta tools)")
        );
        // Manage is a gear icon; the pending preview is carried by its
        // announced name and tooltip rather than visible button text.
        assert_eq!(
            button(&container, "Manage • preview pending")
                .get_attribute("title")
                .as_deref(),
            Some("Manage swarm — change preview pending")
        );

        assert!(!is_disabled(&action(&dialog, "apply")));
        let approved = swarm.change_preview.as_ref().unwrap().constraints.clone();
        for (control, original, max, label) in [
            (
                field(&dialog, "max-live"),
                approved.max_live_agents.to_string(),
                SWARM_MAX_LIVE_AGENTS,
                "Live agents",
            ),
            (
                field(&dialog, "wake-budget"),
                approved
                    .agent_wake_budget
                    .expect("fixture sets a limit")
                    .to_string(),
                SWARM_MAX_AGENT_WAKE_BUDGET,
                "Agent-to-agent turn limit",
            ),
            (
                one(&dialog, "[aria-label='Agents from this profile']"),
                approved.allocations[0].count.to_string(),
                SWARM_MAX_LIVE_AGENTS,
                "Agents from this profile",
            ),
        ] {
            for invalid in [
                String::new(),
                "0".to_owned(),
                (max + 1).to_string(),
                "1.5".to_owned(),
            ] {
                // An empty turn limit is valid: it means no limit.
                if invalid.is_empty() && label == "Agent-to-agent turn limit" {
                    continue;
                }
                if invalid.is_empty() {
                    type_into(&control, &invalid);
                } else {
                    change_control(&control, &invalid);
                }
                settle().await;
                assert!(
                    is_disabled(&action(&dialog, "apply")),
                    "visible invalid numbers cannot apply hidden previous values"
                );
                assert!(is_disabled(&action(&dialog, "preview-change")));
                assert!(
                    text_of(&dialog)
                        .contains(&format!("{label} must be a whole number from 1 to {max}."))
                );
                assert_eq!(
                    control.get_attribute("aria-invalid").as_deref(),
                    Some("true")
                );
                assert_eq!(
                    control
                        .dyn_ref::<web_sys::HtmlInputElement>()
                        .unwrap()
                        .value(),
                    invalid,
                    "invalid visible value is preserved, not clamped or replaced"
                );
                action(&dialog, "apply").click();
                action(&dialog, "preview-change").click();
                settle().await;
                assert!(harness.commands_of("apply_change").is_empty());
                assert_eq!(harness.commands_of("preview_change").len(), 1);
                change_control(&control, &original);
                settle().await;
                assert!(!is_disabled(&action(&dialog, "apply")));
                assert!(!is_disabled(&action(&dialog, "preview-change")));
                assert_eq!(
                    control.get_attribute("aria-invalid").as_deref(),
                    Some("false")
                );
            }
        }
        for (field_name, next, original) in [
            (
                "max-live",
                (approved.max_live_agents + 1).to_string(),
                approved.max_live_agents.to_string(),
            ),
            (
                "wake-budget",
                (approved.agent_wake_budget.expect("fixture sets a limit") + 1).to_string(),
                approved
                    .agent_wake_budget
                    .expect("fixture sets a limit")
                    .to_string(),
            ),
            (
                "wake-budget",
                String::new(),
                approved
                    .agent_wake_budget
                    .expect("fixture sets a limit")
                    .to_string(),
            ),
        ] {
            change_control(&field(&dialog, field_name), &next);
            assert_edited_preview_blocked(&harness, &dialog).await;
            change_control(&field(&dialog, field_name), &original);
            settle().await;
            assert!(!is_disabled(&action(&dialog, "apply")));
        }
        type_into(&field(&dialog, "guidance"), "Keep accessibility in scope");
        assert_edited_preview_blocked(&harness, &dialog).await;
        type_into(&field(&dialog, "guidance"), &approved.shared_guidance);
        settle().await;
        assert!(!is_disabled(&action(&dialog, "apply")));

        let allocation_count = one(&dialog, "[aria-label='Agents from this profile']");
        change_control(
            &allocation_count,
            &(approved.allocations[0].count + 1).to_string(),
        );
        assert_edited_preview_blocked(&harness, &dialog).await;
        change_control(
            &allocation_count,
            &approved.allocations[0].count.to_string(),
        );
        settle().await;
        assert!(!is_disabled(&action(&dialog, "apply")));
        let profile = one(&dialog, "[aria-label='Launch profile']");
        change_control(&profile, "claude-alternate");
        assert_edited_preview_blocked(&harness, &dialog).await;
        change_control(&profile, &approved.allocations[0].launch_profile_id.0);
        settle().await;
        assert!(!is_disabled(&action(&dialog, "apply")));
        change_control(&one(&dialog, ".session-setting-select"), "alternate-model");
        assert_edited_preview_blocked(&harness, &dialog).await;
        change_control(&one(&dialog, ".session-setting-select"), "");
        settle().await;
        // Returning the profile restores the approved absence of an override.
        change_control(&profile, &approved.allocations[0].launch_profile_id.0);
        settle().await;
        assert!(!is_disabled(&action(&dialog, "apply")));
        button(&dialog, "+ Add backend").click();
        assert_edited_preview_blocked(&harness, &dialog).await;
        all(&dialog, "[aria-label='Remove this backend']")[1].click();
        settle().await;
        assert!(!is_disabled(&action(&dialog, "apply")));

        all(&dialog, "input[name='swarm-retirement']")[1].click();
        settle().await;
        let apply = action(&dialog, "apply");
        assert_eq!(text_of(&apply), "Apply preview 7");
        apply.click();
        settle().await;
        assert_eq!(
            harness.commands_of("apply_change"),
            vec![
                json!({"kind": "apply_change", "swarm_id": sid, "preview_revision": 7, "retirement": "interrupt_now"})
            ]
        );
        assert!(modal(&container).is_none(), "apply closes the dialog");

        // A preview made against an older revision cannot be applied.
        swarm.revision = 4;
        harness.swarm(&swarm);
        settle().await;
        button(&container, "Manage • preview pending").click();
        settle().await;
        let dialog = modal(&container).expect("manage dialog");
        assert!(
            text_of(&dialog)
                .contains("The swarm changed since this preview. Preview again before applying.")
        );
        assert!(is_disabled(&action(&dialog, "apply")));
    }
}
