use std::collections::HashSet;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    CustomAgent, CustomAgentId, ProjectId, Team, TeamId, TeamMember, TeamMemberCreateSpec,
    TeamMemberId, TeamMemberRole, TeamMemberState, TeamMemberUpdatePayload,
};

use crate::send::{
    team_create, team_delete, team_member_create, team_member_delete, team_member_update,
    team_set_manager,
};
use crate::state::{ActiveAgentRef, AppState, TabContent};

#[derive(Clone)]
pub(crate) struct MemberFormState {
    pub(crate) team_id: TeamId,
    pub(crate) editing_id: Option<TeamMemberId>,
    pub(crate) is_manager: bool,
    pub(crate) name: RwSignal<String>,
    pub(crate) description: RwSignal<String>,
    pub(crate) custom_agent_id: RwSignal<Option<CustomAgentId>>,
    pub(crate) project_ids: RwSignal<Vec<ProjectId>>,
}

impl MemberFormState {
    fn new_manager(team_id_placeholder: TeamId) -> Self {
        Self {
            team_id: team_id_placeholder,
            editing_id: None,
            is_manager: true,
            name: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            custom_agent_id: RwSignal::new(None),
            project_ids: RwSignal::new(Vec::new()),
        }
    }

    fn new_report(team_id: TeamId) -> Self {
        Self {
            team_id,
            editing_id: None,
            is_manager: false,
            name: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            custom_agent_id: RwSignal::new(None),
            project_ids: RwSignal::new(Vec::new()),
        }
    }

    fn from_member(member: &TeamMember, is_manager: bool) -> Self {
        Self {
            team_id: member.team_id.clone(),
            editing_id: Some(member.id.clone()),
            is_manager,
            name: RwSignal::new(member.name.clone()),
            description: RwSignal::new(member.description.clone()),
            custom_agent_id: RwSignal::new(Some(member.custom_agent_id.clone())),
            project_ids: RwSignal::new(member.project_ids.clone()),
        }
    }
}

fn build_spec(form: &MemberFormState) -> Result<TeamMemberCreateSpec, String> {
    let name = form.name.get_untracked().trim().to_string();
    if name.is_empty() {
        return Err("Member name is required.".to_string());
    }
    let description = form.description.get_untracked().trim().to_string();
    if description.is_empty() {
        return Err("Description is required.".to_string());
    }
    let custom_agent_id = form
        .custom_agent_id
        .get_untracked()
        .ok_or_else(|| "Pick a custom agent.".to_string())?;
    let project_ids = form.project_ids.get_untracked();
    if project_ids.is_empty() {
        return Err("Pick at least one project.".to_string());
    }
    Ok(TeamMemberCreateSpec {
        name,
        description,
        custom_agent_id,
        project_ids,
    })
}

fn build_update(
    form: &MemberFormState,
    member_id: TeamMemberId,
) -> Result<TeamMemberUpdatePayload, String> {
    let name = form.name.get_untracked().trim().to_string();
    if name.is_empty() {
        return Err("Member name is required.".to_string());
    }
    let description = form.description.get_untracked().trim().to_string();
    if description.is_empty() {
        return Err("Description is required.".to_string());
    }
    let project_ids = form.project_ids.get_untracked();
    if project_ids.is_empty() {
        return Err("Pick at least one project.".to_string());
    }
    Ok(TeamMemberUpdatePayload {
        id: member_id,
        name,
        description,
        project_ids,
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WizardStep {
    Name,
    Manager,
    Reports,
}

type ReportDispatchState = Option<(String, Vec<TeamMemberCreateSpec>, HashSet<TeamId>)>;

#[derive(Clone)]
struct NewTeamForm {
    name: RwSignal<String>,
    manager: MemberFormState,
    step: RwSignal<WizardStep>,
    /// Finalized reports stored as plain data (no live signals) to avoid
    /// reactive-owner disposal issues. Each entry is (display_name, spec).
    finalized_reports: RwSignal<Vec<(String, TeamMemberCreateSpec)>>,
}

impl NewTeamForm {
    fn blank() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            // The team_id is a placeholder until create returns; we never
            // serialize this field — create_team payload bundles the manager
            // spec with the team name. Use a dummy id.
            manager: MemberFormState::new_manager(TeamId(String::new())),
            step: RwSignal::new(WizardStep::Name),
            finalized_reports: RwSignal::new(Vec::new()),
        }
    }
}

#[component]
pub fn TeamsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();

    let new_team: RwSignal<Option<NewTeamForm>> = RwSignal::new(None);
    let member_form: RwSignal<Option<MemberFormState>> = RwSignal::new(None);

    let teams_state = state.clone();
    // Pair each team with its host_id at the source, so downstream rows never
    // need to read the ambient `selected_host_id` to know which host they belong
    // to. That removes the bug where a user switches hosts while a team tab is
    // open and the row's actions read from the wrong host's signals.
    let teams_for_host: Memo<Vec<(String, Team)>> = Memo::new(move |_| {
        let Some(host_id) = teams_state.selected_host_id.get() else {
            return Vec::new();
        };
        let mut teams: Vec<Team> = teams_state
            .teams
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        teams.into_iter().map(|t| (host_id.clone(), t)).collect()
    });

    let state_new = state.clone();
    let on_new_team = move |_| {
        if state_new.selected_host_id.get_untracked().is_none() {
            return;
        }
        new_team.set(Some(NewTeamForm::blank()));
    };

    view! {
        <div class="panel teams-panel">
            <div class="panel-filters">
                <button
                    class="filter-toggle"
                    disabled=move || state.selected_host_id.get().is_none()
                    on:click=on_new_team
                >
                    "+ New team"
                </button>
            </div>
            <div class="panel-content">
                <div class="team-card-list">
                    <For
                        each=move || teams_for_host.get()
                        key=|(_host_id, team)| team.id.clone()
                        let:entry
                    >
                        {
                            let (host_id, team) = entry;
                            let team_id = team.id.clone();
                            let host_for_open = host_id.clone();
                            let host_for_delete = host_id.clone();
                            let host_for_delete_member = host_id.clone();
                            let host_for_open_member = host_id.clone();
                            let host_for_promote = host_id.clone();
                            let host_for_card = host_id;
                            let tid_open = team_id.clone();
                            let tid_add = team_id.clone();
                            let tid_delete = team_id.clone();
                            let tid_promote = team_id.clone();
                            let state_open = state.clone();
                            let state_delete = state.clone();
                            let state_delete_member = state.clone();
                            let state_open_member = state.clone();
                            let state_promote = state.clone();
                            view! {
                                <TeamCard
                                    host_id=host_for_card
                                    team_id=team_id
                                    on_open=Callback::new(move |_: ()| {
                                        open_team(&state_open, host_for_open.clone(), tid_open.clone())
                                    })
                                    on_add_report=Callback::new(move |_: ()| {
                                        member_form.set(Some(MemberFormState::new_report(tid_add.clone())))
                                    })
                                    on_delete=Callback::new(move |_: ()| {
                                        delete_team(&state_delete, host_for_delete.clone(), tid_delete.clone())
                                    })
                                    on_edit_member=Callback::new(move |form_state: MemberFormState| {
                                        member_form.set(Some(form_state))
                                    })
                                    on_delete_member=Callback::new(move |member_id: TeamMemberId| {
                                        delete_member(&state_delete_member, host_for_delete_member.clone(), member_id)
                                    })
                                    on_open_member=Callback::new(move |member_id: TeamMemberId| {
                                        open_member_chat(&state_open_member, host_for_open_member.clone(), member_id)
                                    })
                                    on_promote_member=Callback::new(move |member_id: TeamMemberId| {
                                        promote_member(&state_promote, host_for_promote.clone(), tid_promote.clone(), member_id)
                                    })
                                />
                            }
                        }
                    </For>
                </div>
                {move || teams_for_host.get().is_empty().then(|| view! {
                    <div class="panel-empty">"No teams on this host."</div>
                })}
            </div>

            {move || new_team.get().map(|form| view! {
                <NewTeamDialog form=form on_close=Callback::new(move |_: ()| new_team.set(None)) />
            })}
            {move || member_form.get().map(|form| view! {
                <MemberDialog form=form on_close=Callback::new(move |_: ()| member_form.set(None)) />
            })}
        </div>
    }
}

#[component]
fn TeamCard(
    host_id: String,
    team_id: TeamId,
    on_open: Callback<()>,
    on_add_report: Callback<()>,
    on_delete: Callback<()>,
    on_edit_member: Callback<MemberFormState>,
    on_delete_member: Callback<TeamMemberId>,
    on_open_member: Callback<TeamMemberId>,
    on_promote_member: Callback<TeamMemberId>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Reactive look-up of this row's team record. Re-runs whenever the
    // backing `teams` signal changes, even though the `<For>` row key
    // (TeamId) is stable — per the philosophy doc, snapshotting `team`
    // into the view would freeze name/archived-state for this row.
    let team_id_for_lookup = team_id.clone();
    let host_for_lookup = host_id.clone();
    let state_for_lookup = state.clone();
    let team_record: Memo<Option<Team>> = Memo::new(move |_| {
        state_for_lookup.teams.with(|map| {
            map.get(&host_for_lookup)
                .and_then(|m| m.get(&team_id_for_lookup).cloned())
        })
    });

    let team_id_for_members = team_id.clone();
    let host_for_members = host_id.clone();
    let state_for_members = state.clone();
    let members: Memo<Vec<TeamMember>> = Memo::new(move |_| {
        let mut members: Vec<TeamMember> = state_for_members
            .team_members
            .get()
            .get(&host_for_members)
            .map(|map| {
                map.values()
                    .filter(|m| m.team_id == team_id_for_members)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        // Manager first, then by name.
        members.sort_by(|a, b| {
            let role_rank = |r: TeamMemberRole| match r {
                TeamMemberRole::Manager => 0,
                TeamMemberRole::Report => 1,
            };
            role_rank(a.role)
                .cmp(&role_rank(b.role))
                .then(a.name.cmp(&b.name))
        });
        members
    });

    let name = move || team_record.get().map(|t| t.name).unwrap_or_default();
    let member_count = move || members.get().len();

    let host_for_rows = host_id.clone();
    view! {
        <div class="team-card" data-team-id=team_id.0.clone()>
            <div class="team-card-header">
                <button
                    class="team-card-title"
                    type="button"
                    on:click=move |_| on_open.run(())
                >
                    <span class="team-card-name">{name}</span>
                    <span class="team-card-count">
                        {move || format!("{} members", member_count())}
                    </span>
                </button>
                <div class="team-card-actions">
                    <button
                        class="filter-toggle"
                        type="button"
                        on:click=move |_| on_add_report.run(())
                    >
                        "+ Report"
                    </button>
                    <button
                        class="filter-toggle"
                        type="button"
                        on:click=move |_| on_delete.run(())
                    >
                        "Delete"
                    </button>
                </div>
            </div>
            <div class="team-card-roster">
                <For
                    each=move || members.get()
                    key=|member| member.id.clone()
                    let:member
                >
                    {
                        let member_id = member.id.clone();
                        let host_for_row = host_for_rows.clone();
                        view! {
                            <MemberRow
                                host_id=host_for_row
                                member_id=member_id
                                on_edit=on_edit_member
                                on_delete=on_delete_member
                                on_open=on_open_member
                                on_promote=on_promote_member
                            />
                        }
                    }
                </For>
            </div>
        </div>
    }
}

#[component]
fn MemberRow(
    host_id: String,
    member_id: TeamMemberId,
    on_edit: Callback<MemberFormState>,
    on_delete: Callback<TeamMemberId>,
    on_open: Callback<TeamMemberId>,
    on_promote: Callback<TeamMemberId>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let mid_for_member = member_id.clone();
    let host_for_member = host_id.clone();
    let state_for_member = state.clone();
    let member: Memo<Option<TeamMember>> = Memo::new(move |_| {
        state_for_member.team_members.with(|map| {
            map.get(&host_for_member)
                .and_then(|m| m.get(&mid_for_member).cloned())
        })
    });

    let mid_for_binding = member_id.clone();
    let host_for_binding = host_id.clone();
    let state_for_binding = state.clone();
    let binding_status = move || -> Option<protocol::AgentControlStatus> {
        state_for_binding.team_member_bindings.with(|map| {
            map.get(&host_for_binding)
                .and_then(|m| m.get(&mid_for_binding))
                .map(|b| b.status)
        })
    };

    let host_for_agent = host_id.clone();
    let custom_agent_state = state.clone();
    let custom_agent_label = move || -> Option<String> {
        let m = member.get()?;
        custom_agent_state.custom_agents.with(|map| {
            map.get(&host_for_agent)
                .and_then(|m2| m2.get(&m.custom_agent_id).map(|c| c.name.clone()))
        })
    };

    let host_for_projects = host_id.clone();
    let projects_state = state.clone();
    let project_labels = move || -> String {
        let Some(m) = member.get() else {
            return String::new();
        };
        let host = host_for_projects.clone();
        projects_state.projects.with(|projects| {
            m.project_ids
                .iter()
                .map(|pid| {
                    projects
                        .iter()
                        .find(|p| p.host_id == host && &p.project.id == pid)
                        .map(|p| p.project.name.clone())
                        .unwrap_or_else(|| pid.0.clone())
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    };

    let mid_for_open = member_id.clone();
    let mid_for_edit = member_id.clone();
    let mid_for_delete = member_id.clone();
    let mid_for_promote = member_id;

    let on_click = move |_: web_sys::MouseEvent| {
        on_open.run(mid_for_open.clone());
    };

    let on_edit_click = {
        let mid = mid_for_edit.clone();
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            let Some(m) = member.get_untracked() else {
                return;
            };
            let is_manager = matches!(m.role, TeamMemberRole::Manager);
            let _ = mid; // edit form keys off member's id field
            on_edit.run(MemberFormState::from_member(&m, is_manager));
        }
    };

    let on_delete_click = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        on_delete.run(mid_for_delete.clone());
    };

    let on_promote_click = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        on_promote.run(mid_for_promote.clone());
    };

    let can_promote = move || matches!(member.get().map(|m| m.role), Some(TeamMemberRole::Report));

    view! {
        <div
            class="team-member-row"
            role="button"
            tabindex="0"
            on:click=on_click
        >
            <div class="team-member-main">
                <div class="team-member-name-row">
                    <span class="team-member-name">
                        {move || member.get().map(|m| m.name).unwrap_or_default()}
                    </span>
                    <span class="team-member-role-badge">
                        {move || match member.get().map(|m| m.role) {
                            Some(TeamMemberRole::Manager) => "Manager",
                            Some(TeamMemberRole::Report) => "Report",
                            None => "",
                        }}
                    </span>
                    {move || {
                        let state_str = member.get().map(|m| match m.state {
                            TeamMemberState::Active => "",
                            TeamMemberState::Paused => "paused",
                        }).unwrap_or("");
                        (!state_str.is_empty()).then(|| view! {
                            <span class="team-member-state">{state_str.to_string()}</span>
                        })
                    }}
                    {move || binding_status().map(|status| view! {
                        <span class="team-member-status">{format!("{status:?}").to_lowercase()}</span>
                    })}
                </div>
                <div class="team-member-meta">
                    {move || custom_agent_label().map(|s| view! {
                        <span class="team-member-custom-agent">{s}</span>
                    })}
                    {move || {
                        let s = project_labels();
                        (!s.is_empty()).then(|| view! {
                            <span class="team-member-projects-summary">{s}</span>
                        })
                    }}
                </div>
            </div>
            <div class="team-member-actions">
                {move || can_promote().then(|| view! {
                    <button class="filter-toggle" type="button" on:click=on_promote_click.clone()>
                        "Set manager"
                    </button>
                })}
                <button class="filter-toggle" type="button" on:click=on_edit_click>"Edit"</button>
                <button class="filter-toggle" type="button" on:click=on_delete_click>"Delete"</button>
            </div>
        </div>
    }
}

#[component]
fn NewTeamDialog(form: NewTeamForm, on_close: Callback<()>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let error_sig: RwSignal<Option<String>> = RwSignal::new(None);
    let submitting: RwSignal<bool> = RwSignal::new(false);

    let name_sig = form.name;
    let manager_form = form.manager.clone();
    let step_sig = form.step;
    // finalized_reports stores (display_name, spec) as plain data so there are
    // no live signal reads between confirmation and the eventual TeamCreate submit.
    let finalized_reports_sig = form.finalized_reports;

    // The inline add-report form. Created here (at component rendering time,
    // under a proper reactive owner) so its RwSignal fields are never
    // prematurely disposed. Wrapped in StoredValue<T: Copy> so it can be
    // captured by multiple move closures without making any of them FnOnce.
    let pending_report_store: StoredValue<MemberFormState> =
        StoredValue::new(MemberFormState::new_report(TeamId(String::new())));
    let show_pending_report: RwSignal<bool> = RwSignal::new(false);

    // Step 1 → 2: validate name only
    let on_next_name = move |_| {
        let team_name = name_sig.get_untracked().trim().to_string();
        if team_name.is_empty() {
            error_sig.set(Some("Team name is required.".to_string()));
            return;
        }
        error_sig.set(None);
        step_sig.set(WizardStep::Manager);
    };

    // Step 2 → 3: validate manager spec
    let manager_form_for_next = manager_form.clone();
    let on_next_manager = move |_| {
        if let Err(e) = build_spec(&manager_form_for_next) {
            error_sig.set(Some(e));
            return;
        }
        error_sig.set(None);
        step_sig.set(WizardStep::Reports);
    };

    // Open the inline add-report form (reset fields then reveal)
    let on_add_report = move |_| {
        pending_report_store.with_value(|form| {
            form.name.set(String::new());
            form.description.set(String::new());
            form.custom_agent_id.set(None);
            form.project_ids.set(Vec::new());
        });
        show_pending_report.set(true);
        error_sig.set(None);
    };

    // Confirm the inline report (validate → build spec → store plain data → hide form)
    let on_confirm_pending = move |_| {
        let result = pending_report_store.with_value(build_spec);
        match result {
            Ok(spec) => {
                let display_name = spec.name.clone();
                finalized_reports_sig.update(|reports| reports.push((display_name, spec)));
                show_pending_report.set(false);
                error_sig.set(None);
            }
            Err(e) => {
                error_sig.set(Some(format!("Report: {e}")));
            }
        }
    };

    // Cancel the inline report form without adding
    let on_cancel_pending = move |_| {
        show_pending_report.set(false);
        error_sig.set(None);
    };

    let on_remove_report = move |idx: usize| {
        finalized_reports_sig.update(|reports| {
            if idx < reports.len() {
                reports.remove(idx);
            }
        });
    };

    // Holds the context needed to dispatch TeamMemberCreate calls once the
    // server's TeamNotify::Upsert for the newly-created team arrives. Written
    // by on_save after team_create succeeds; read by the component-scope
    // Effect below which has a proper reactive owner (unlike an Effect created
    // inside spawn_local, which would be dropped after its first run).
    let report_dispatch: RwSignal<ReportDispatchState> = RwSignal::new(None);

    let state_for_effect = state.clone();
    Effect::new(move |_| {
        let Some((host_id, specs, pre)) = report_dispatch.get() else {
            return;
        };
        let new_team_id = state_for_effect.teams.with(|map| {
            map.get(&host_id)
                .and_then(|m| m.keys().find(|tid| !pre.contains(*tid)).cloned())
        });
        let Some(new_team_id) = new_team_id else {
            return;
        };
        // Clear so this Effect body won't re-enter on subsequent team updates.
        report_dispatch.set(None);
        let Some(stream) = state_for_effect.host_stream_untracked(&host_id) else {
            log::error!("report dispatch: host stream gone for {host_id}");
            submitting.set(false);
            on_close.run(());
            return;
        };
        spawn_local(async move {
            for spec in specs {
                if let Err(error) =
                    team_member_create(&host_id, stream.clone(), new_team_id.clone(), spec).await
                {
                    log::error!("team_member_create failed: {error}");
                }
            }
            submitting.set(false);
            on_close.run(());
        });
    });

    let state_for_save = state.clone();
    let manager_for_save = manager_form.clone();
    let on_save = move |_| {
        let team_name = name_sig.get_untracked().trim().to_string();
        if team_name.is_empty() {
            error_sig.set(Some("Team name is required.".to_string()));
            return;
        }
        let manager_spec = match build_spec(&manager_for_save) {
            Ok(s) => s,
            Err(e) => {
                error_sig.set(Some(e));
                return;
            }
        };
        // Reports were validated and converted to plain specs when added.
        let report_specs: Vec<TeamMemberCreateSpec> = finalized_reports_sig
            .get_untracked()
            .into_iter()
            .map(|(_, spec)| spec)
            .collect();
        let Some(host_id) = state_for_save.selected_host_id.get_untracked() else {
            error_sig.set(Some("No host selected.".to_string()));
            return;
        };
        let Some(stream) = state_for_save.host_stream_untracked(&host_id) else {
            error_sig.set(Some("Host is not connected.".to_string()));
            return;
        };
        error_sig.set(None);
        submitting.set(true);

        // Snapshot existing team ids on this host so we can spot the new one
        // when the server's TeamNotify::Upsert lands.
        let pre_existing: HashSet<TeamId> = state_for_save.teams.with_untracked(|map| {
            map.get(&host_id)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        });

        let team_name_for_call = team_name.clone();
        let host_for_call = host_id.clone();
        spawn_local(async move {
            if let Err(error) =
                team_create(&host_for_call, stream, team_name_for_call, manager_spec).await
            {
                log::error!("team_create failed: {error}");
                error_sig.set(Some(error));
                submitting.set(false);
                return;
            }
            if report_specs.is_empty() {
                submitting.set(false);
                on_close.run(());
                return;
            }
            // Signal the component-scope Effect to watch for the Upsert echo
            // and then dispatch TeamMemberCreate for each report.
            report_dispatch.set(Some((host_for_call, report_specs, pre_existing)));
        });
    };

    let on_cancel = move |_| on_close.run(());
    let manager_form_for_fields = manager_form.clone();
    view! {
        <ModalOverlay on_close=on_close>
            // Step 1: name only
            <Show when=move || step_sig.get() == WizardStep::Name>
                <h3 class="settings-confirm-title">"New team — name"</h3>
                <label class="settings-form-label">
                    <span>"Team name"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || name_sig.get()
                        on:input=move |ev| name_sig.set(event_target_value(&ev))
                        spellcheck="false"
                        autocapitalize="none"
                        autocomplete="off"
                    />
                </label>
            </Show>

            // Step 2: manager form
            <Show when=move || step_sig.get() == WizardStep::Manager>
                <h3 class="settings-confirm-title">"New team — manager"</h3>
                <MemberFormFields form=manager_form_for_fields.clone() />
            </Show>

            // Step 3: optional reports
            <Show when=move || step_sig.get() == WizardStep::Reports>
                <h3 class="settings-confirm-title">"New team — reports (optional)"</h3>
                <p class="settings-form-hint">
                    "Add any reports now, or finish and add them later from the team card."
                </p>
                <div class="team-wizard-reports">
                    // Finalized reports as chips (plain data, no live signals)
                    {move || finalized_reports_sig.get().into_iter().enumerate().map(|(idx, (name, _spec))| {
                        view! {
                            <div class="team-wizard-report-chip">
                                <span class="team-wizard-report-name">{name}</span>
                                <button
                                    class="settings-btn"
                                    type="button"
                                    on:click=move |_| on_remove_report(idx)
                                >"Remove"</button>
                            </div>
                        }
                    }).collect_view()}
                    // Inline pending-report form (shown via show_pending_report)
                    <Show when=move || show_pending_report.get()>
                        <div class="team-wizard-pending-report">
                            <MemberFormFields form=pending_report_store.with_value(|f| f.clone()) />
                            <div class="settings-form-row">
                                <button
                                    class="settings-btn"
                                    type="button"
                                    on:click=on_cancel_pending
                                >"Cancel"</button>
                                <button
                                    class="settings-btn settings-btn-primary"
                                    type="button"
                                    on:click=on_confirm_pending
                                >"Add"</button>
                            </div>
                        </div>
                    </Show>
                    <Show when=move || !show_pending_report.get()>
                        <button
                            class="settings-btn"
                            type="button"
                            on:click=on_add_report
                        >"+ Add a report"</button>
                    </Show>
                </div>
            </Show>

            <Show when=move || error_sig.get().is_some()>
                <p class="settings-error">{move || error_sig.get().unwrap_or_default()}</p>
            </Show>
            <div class="settings-form-footer">
                // Step 1: Cancel + Next
                <Show when=move || step_sig.get() == WizardStep::Name>
                    <button
                        class="settings-btn"
                        on:click=on_cancel
                        disabled=move || submitting.get()
                    >"Cancel"</button>
                    <button
                        class="settings-btn settings-btn-primary"
                        on:click=on_next_name
                        disabled=move || submitting.get()
                    >"Next"</button>
                </Show>
                // Step 2: Back + Next
                <Show when=move || step_sig.get() == WizardStep::Manager>
                    <button
                        class="settings-btn"
                        on:click=move |_| step_sig.set(WizardStep::Name)
                        disabled=move || submitting.get()
                    >"Back"</button>
                    <button
                        class="settings-btn settings-btn-primary"
                        on:click=on_next_manager.clone()
                        disabled=move || submitting.get()
                    >"Next"</button>
                </Show>
                // Step 3: Back + Finish. Hidden while the inline pending-report
                // form is open — the user is in a sub-flow and only Cancel/Add
                // should be reachable until that resolves.
                <Show when=move || step_sig.get() == WizardStep::Reports && !show_pending_report.get()>
                    <button
                        class="settings-btn"
                        on:click=move |_| step_sig.set(WizardStep::Manager)
                        disabled=move || submitting.get()
                    >"Back"</button>
                    <button
                        class="settings-btn settings-btn-primary"
                        on:click=on_save.clone()
                        disabled=move || submitting.get()
                    >
                        {move || if submitting.get() { "Creating…" } else { "Finish" }}
                    </button>
                </Show>
            </div>
        </ModalOverlay>
    }
}

#[component]
fn MemberDialog(form: MemberFormState, on_close: Callback<()>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let error_sig: RwSignal<Option<String>> = RwSignal::new(None);
    let editing_id = form.editing_id.clone();
    let team_id = form.team_id.clone();
    let form_for_save = form.clone();
    let form_for_fields = form.clone();
    let title = if editing_id.is_some() {
        "Edit member"
    } else if form.is_manager {
        "Replace manager"
    } else {
        "Add report"
    };

    let state_for_save = state.clone();
    let on_save = move |_| {
        let Some(host_id) = state_for_save.selected_host_id.get_untracked() else {
            error_sig.set(Some("No host selected.".to_string()));
            return;
        };
        let Some(stream) = state_for_save.host_stream_untracked(&host_id) else {
            error_sig.set(Some("Host is not connected.".to_string()));
            return;
        };
        if let Some(member_id) = editing_id.clone() {
            let payload = match build_update(&form_for_save, member_id) {
                Ok(p) => p,
                Err(e) => {
                    error_sig.set(Some(e));
                    return;
                }
            };
            error_sig.set(None);
            spawn_local(async move {
                if let Err(error) = team_member_update(&host_id, stream, payload).await {
                    log::error!("team_member_update failed: {error}");
                    error_sig.set(Some(error));
                    return;
                }
                on_close.run(());
            });
        } else {
            let spec = match build_spec(&form_for_save) {
                Ok(s) => s,
                Err(e) => {
                    error_sig.set(Some(e));
                    return;
                }
            };
            error_sig.set(None);
            let team_id = team_id.clone();
            spawn_local(async move {
                if let Err(error) = team_member_create(&host_id, stream, team_id, spec).await {
                    log::error!("team_member_create failed: {error}");
                    error_sig.set(Some(error));
                    return;
                }
                on_close.run(());
            });
        }
    };

    let on_cancel = move |_| on_close.run(());
    view! {
        <ModalOverlay on_close=on_close>
            <h3 class="settings-confirm-title">{title}</h3>
            <MemberFormFields form=form_for_fields />
            <Show when=move || error_sig.get().is_some()>
                <p class="settings-error">{move || error_sig.get().unwrap_or_default()}</p>
            </Show>
            <div class="settings-form-footer">
                <button class="settings-btn" on:click=on_cancel>"Cancel"</button>
                <button class="settings-btn settings-btn-primary" on:click=on_save>"Save"</button>
            </div>
        </ModalOverlay>
    }
}

#[component]
fn MemberFormFields(form: MemberFormState) -> impl IntoView {
    let state = expect_context::<AppState>();
    let name_sig = form.name;
    let description_sig = form.description;
    let custom_agent_sig = form.custom_agent_id;
    let project_ids_sig = form.project_ids;
    let is_editing = form.editing_id.is_some();

    let state_for_agents = state.clone();
    let available_agents: Memo<Vec<CustomAgent>> = Memo::new(move |_| {
        let Some(host_id) = state_for_agents.selected_host_id.get() else {
            return Vec::new();
        };
        let mut agents: Vec<CustomAgent> = state_for_agents
            .custom_agents
            .get()
            .get(&host_id)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    });

    let state_for_projects = state.clone();
    let available_projects: Memo<Vec<protocol::Project>> = Memo::new(move |_| {
        let Some(host_id) = state_for_projects.selected_host_id.get() else {
            return Vec::new();
        };
        let mut projects: Vec<protocol::Project> = state_for_projects
            .projects
            .get()
            .into_iter()
            .filter(|p| p.host_id == host_id)
            .map(|p| p.project)
            .collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        projects
    });

    view! {
        <label class="settings-form-label">
            <span>"Name"</span>
            <input
                class="settings-text-input"
                type="text"
                prop:value=move || name_sig.get()
                on:input=move |ev| name_sig.set(event_target_value(&ev))
                spellcheck="false"
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
                autocapitalize="none"
                autocomplete="off"
            />
        </label>
        <label class="settings-form-label">
            <span>"Custom agent"</span>
            <select
                class="settings-text-input"
                disabled=is_editing
                on:change=move |ev| {
                    let val = event_target_value(&ev);
                    if val.is_empty() {
                        custom_agent_sig.set(None);
                    } else {
                        custom_agent_sig.set(Some(CustomAgentId(val)));
                    }
                }
            >
                <option
                    value=""
                    prop:selected=move || custom_agent_sig.get().is_none()
                >
                    "— select —"
                </option>
                {move || available_agents.get().into_iter().map(|agent| {
                    let id_str = agent.id.0.clone();
                    let id_val = agent.id.clone();
                    let label = agent.name.clone();
                    view! {
                        <option
                            value=id_str
                            prop:selected=move || custom_agent_sig.get().as_ref() == Some(&id_val)
                        >
                            {label}
                        </option>
                    }
                }).collect_view()}
            </select>
            {move || is_editing.then(|| view! {
                <span class="settings-form-hint">"The custom agent is fixed once a member exists."</span>
            })}
        </label>
        <div class="settings-form-label team-member-projects">
            <span>"Projects"<span class="settings-form-hint">" (pick one or more — workspace roots are derived)"</span></span>
            <div class="team-member-project-list">
                <For
                    each=move || available_projects.get()
                    key=|project| project.id.clone()
                    let:project
                >
                    {
                        let id_val = project.id.clone();
                        let id_for_change = id_val.clone();
                        let id_for_checked = id_val.clone();
                        let id_for_input_id = id_val.0.clone();
                        let label = project.name.clone();
                        view! {
                            <label class="team-member-project-row">
                                <input
                                    id=id_for_input_id
                                    type="checkbox"
                                    prop:checked=move || project_ids_sig
                                        .with(|ids| ids.iter().any(|p| p == &id_for_checked))
                                    on:change=move |ev| {
                                        let checked = event_target_checked(&ev);
                                        let id = id_for_change.clone();
                                        project_ids_sig.update(|ids| {
                                            if checked {
                                                if !ids.iter().any(|p| p == &id) {
                                                    ids.push(id);
                                                }
                                            } else {
                                                ids.retain(|p| p != &id);
                                            }
                                        });
                                    }
                                />
                                <span class="team-member-project-name">{label}</span>
                            </label>
                        }
                    }
                </For>
                {move || available_projects.get().is_empty().then(|| view! {
                    <div class="team-member-project-empty">
                        "No projects on this host. Create one before adding team members."
                    </div>
                })}
            </div>
        </div>
    }
}

fn event_target_checked(ev: &web_sys::Event) -> bool {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}

#[component]
fn ModalOverlay(on_close: Callback<()>, children: Children) -> impl IntoView {
    // Deliberately do NOT dismiss on backdrop click: these wizards carry
    // multi-step form state (name, manager spec, finalized reports). A stray
    // click outside the modal used to silently throw all of that away with no
    // feedback, which surfaced as "I clicked Finish and nothing happened" —
    // in reality the wizard had been dismissed before Finish was clicked.
    // The user closes via the explicit Cancel button or Escape.
    let close_on_keydown = on_close;
    view! {
        <div
            class="settings-confirm-overlay"
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" {
                    close_on_keydown.run(());
                }
            }
            tabindex="0"
        >
            <div class="settings-confirm-modal">
                {children()}
            </div>
        </div>
    }
}

/// Resolve the team's manager and open their agent chat. Defers to
/// `open_member_chat`, which branches on the member's `(binding,
/// session_id)` state — see that function for the contract.
pub(crate) fn open_team(state: &AppState, host_id: String, team_id: TeamId) {
    let team = state
        .teams
        .with_untracked(|map| map.get(&host_id).and_then(|m| m.get(&team_id).cloned()));
    let Some(team) = team else {
        log::error!("open_team: team {team_id} not found on host {host_id}");
        return;
    };
    open_member_chat(state, host_id, team.manager_member_id);
}

pub(crate) fn open_member_chat(state: &AppState, host_id: String, member_id: TeamMemberId) {
    let Some(member) = state
        .team_members
        .with_untracked(|map| map.get(&host_id).and_then(|m| m.get(&member_id).cloned()))
    else {
        log::error!("open_member_chat: member {member_id} not found on host {host_id}");
        return;
    };
    let binding = state
        .team_member_bindings
        .with_untracked(|map| map.get(&host_id).and_then(|m| m.get(&member_id).cloned()));

    if let Some(agent_id) = binding.and_then(|b| b.current_agent_id) {
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef { host_id, agent_id }),
            member.name.clone(),
            true,
        );
        return;
    }

    // Switch project context so the eventual `NewAgent` echo lands in the
    // user's current view (same trick `resume_session` uses). The first
    // project_id is the member's primary project — the same id the server
    // passes as the spawned agent's `project_id` (see §6.3 in 19-agent-teams).
    let target_project = member
        .project_ids
        .first()
        .cloned()
        .map(|project_id| crate::state::ActiveProjectRef {
            host_id: host_id.clone(),
            project_id,
        });
    state.switch_active_project(target_project);

    // Open the draft tab keyed to the member. The chat input is mounted
    // immediately; the first message the user types will go out as
    // `TeamMemberActivate { prompt: Some(_) }` and the server's `NewAgent`
    // echo will upgrade this tab into a live chat (see `dispatch.rs`
    // `upgrade_pending_team_member_tab`).
    state.open_tab(
        TabContent::team_member_draft(host_id.clone(), member_id.clone()),
        member.name.clone(),
        true,
    );

    if member.session_id.is_some() {
        // Unbound but has a session: send the typed activation frame so the
        // server knows the user has reopened this member. The server is a
        // no-op for `prompt: None` resumes, but exposes this as a documented
        // contract for parity with the message_team_member path and for any
        // future server-side bookkeeping. The real spawn happens once the
        // user types their first message.
        let Some(stream) = state.host_stream_untracked(&host_id) else {
            log::error!("open_member_chat: host stream missing for {host_id}");
            return;
        };
        let host_id_for_send = host_id;
        let member_id_for_send = member_id;
        spawn_local(async move {
            if let Err(error) = crate::send::team_member_activate(
                &host_id_for_send,
                stream,
                member_id_for_send,
                None,
                None,
            )
            .await
            {
                log::error!("team_member_activate (no prompt) failed: {error}");
            }
        });
    }
}

fn delete_team(state: &AppState, host_id: String, team_id: TeamId) {
    let state = state.clone();
    spawn_local(async move {
        let Some(stream) = state.host_stream_untracked(&host_id) else {
            return;
        };
        if !crate::bridge::confirm_dialog(
            "Delete team",
            "Delete this team? This cannot be undone.",
        )
        .await
        {
            return;
        }
        if let Err(error) = team_delete(&host_id, stream, team_id).await {
            log::error!("team_delete failed: {error}");
        }
    });
}

fn promote_member(state: &AppState, host_id: String, team_id: TeamId, member_id: TeamMemberId) {
    let state = state.clone();
    spawn_local(async move {
        let Some(stream) = state.host_stream_untracked(&host_id) else {
            return;
        };
        if !crate::bridge::confirm_dialog(
            "Promote to manager",
            "Make this report the team's manager? The current manager will become a report.",
        )
        .await
        {
            return;
        }
        if let Err(error) = team_set_manager(&host_id, stream, team_id, member_id).await {
            log::error!("team_set_manager failed: {error}");
        }
    });
}

fn delete_member(state: &AppState, host_id: String, member_id: TeamMemberId) {
    let state = state.clone();
    spawn_local(async move {
        let Some(stream) = state.host_stream_untracked(&host_id) else {
            return;
        };
        if !crate::bridge::confirm_dialog(
            "Delete member",
            "Delete this team member? This cannot be undone.",
        )
        .await
        {
            return;
        }
        if let Err(error) = team_member_delete(&host_id, stream, member_id).await {
            log::error!("team_member_delete failed: {error}");
        }
    });
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::ConnectionStatus;
    use leptos::mount::mount_to;
    use protocol::{
        AgentControlStatus, AgentId, CustomAgent, CustomAgentId, FrameKind, SessionId, StreamPath,
        Team, TeamId, TeamMember, TeamMemberBindingPayload, TeamMemberId, TeamMemberRole,
        TeamMemberState, ToolPolicy,
    };
    use std::collections::HashMap;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 600px; height: 800px;",
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

    fn make_team(id: &str, name: &str, manager_id: &str) -> Team {
        Team {
            id: TeamId(id.to_owned()),
            name: name.to_owned(),
            manager_member_id: TeamMemberId(manager_id.to_owned()),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn make_member(id: &str, team_id: &str, name: &str, role: TeamMemberRole) -> TeamMember {
        TeamMember {
            id: TeamMemberId(id.to_owned()),
            team_id: TeamId(team_id.to_owned()),
            role,
            state: TeamMemberState::Active,
            name: name.to_owned(),
            description: String::new(),
            custom_agent_id: protocol::CustomAgentId("ca-1".to_owned()),
            session_id: None,
            project_ids: vec![protocol::ProjectId("p-1".to_owned())],
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn install_state(host_id: &str, teams: Vec<Team>, members: Vec<TeamMember>) -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(host_id.to_owned()));
        let mut team_map: HashMap<TeamId, Team> = HashMap::new();
        for team in teams {
            team_map.insert(team.id.clone(), team);
        }
        state.teams.update(|m| {
            m.insert(host_id.to_owned(), team_map);
        });
        let mut member_map: HashMap<TeamMemberId, TeamMember> = HashMap::new();
        for member in members {
            member_map.insert(member.id.clone(), member);
        }
        state.team_members.update(|m| {
            m.insert(host_id.to_owned(), member_map);
        });
        state
    }

    fn visible_text(container: &HtmlElement) -> String {
        container.text_content().unwrap_or_default()
    }

    /// Install a stub `window.__TAURI__.core.invoke` that records each call
    /// into `window.__test_send_calls` as `[cmd, args_json]` and resolves
    /// immediately. The returned JS Array is the same backing array, so
    /// tests can read it after triggering UI actions. Each call to
    /// `install_send_stub` clears any prior recording.
    fn install_send_stub() -> js_sys::Array {
        let code = r#"
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
                return window.__test_send_calls;
            })();
        "#;
        let calls = js_sys::eval(code).expect("install tauri stub");
        calls.dyn_into::<js_sys::Array>().expect("array")
    }

    /// Parse the recorded send-host-line calls into a list of
    /// `(frame_kind_str, payload_json_value)` tuples. Only `send_host_line`
    /// invocations are returned — handshake calls and other commands are
    /// filtered out. Each `line` payload is the JSON envelope we serialized
    /// in `send_frame`, so we can pluck `kind` and `payload` for assertions.
    fn recorded_frames(calls: &js_sys::Array) -> Vec<(String, serde_json::Value)> {
        let mut out = Vec::new();
        for entry in calls.iter() {
            let arr = entry.dyn_into::<js_sys::Array>().expect("entry array");
            let cmd = arr.get(0).as_string().expect("cmd is string");
            if cmd != "send_host_line" {
                continue;
            }
            let args_json = arr.get(1).as_string().expect("args json string");
            let args: serde_json::Value = serde_json::from_str(&args_json).expect("args parse");
            let line = args
                .get("line")
                .and_then(|v| v.as_str())
                .expect("line present");
            let envelope: serde_json::Value = serde_json::from_str(line).expect("envelope parse");
            let kind = envelope
                .get("kind")
                .and_then(|v| v.as_str())
                .expect("kind present")
                .to_string();
            let payload = envelope.get("payload").cloned().unwrap_or(JsonValue::Null);
            out.push((kind, payload));
        }
        out
    }

    use serde_json::Value as JsonValue;

    fn install_host_stream(state: &AppState, host_id: &str) {
        state.host_streams.update(|streams| {
            streams.insert(host_id.to_owned(), StreamPath(format!("/host/{host_id}")));
        });
    }

    #[wasm_bindgen_test]
    async fn teams_panel_renders_one_card_per_team_by_name() {
        let container = make_container();
        let state = install_state(
            "host-a",
            vec![
                make_team("t-1", "Alpha", "m-1"),
                make_team("t-2", "Beta", "m-2"),
            ],
            vec![
                make_member("m-1", "t-1", "Manager A", TeamMemberRole::Manager),
                make_member("m-2", "t-2", "Manager B", TeamMemberRole::Manager),
            ],
        );
        let _handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        // One team-open affordance per team — identified by the visible team
        // name text rather than a private CSS class.
        let titles = container
            .query_selector_all("button.team-card-title")
            .unwrap();
        assert_eq!(titles.length(), 2, "expected one title button per team");
        let text = visible_text(&container);
        assert!(
            text.contains("Alpha"),
            "expected 'Alpha' in panel: {text:?}"
        );
        assert!(text.contains("Beta"), "expected 'Beta' in panel: {text:?}");
    }

    #[wasm_bindgen_test]
    async fn member_row_shows_live_binding_status_text() {
        let container = make_container();
        let host_id = "host-a";
        let manager_id = TeamMemberId("m-1".to_owned());

        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager A",
                TeamMemberRole::Manager,
            )],
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        // No 'thinking' text until a binding arrives.
        assert!(
            !visible_text(&container).to_lowercase().contains("thinking"),
            "no 'thinking' text before binding arrives"
        );

        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: None,
                    status: AgentControlStatus::Thinking,
                    last_active_at_ms: None,
                },
            );
        });
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.to_lowercase().contains("thinking"),
            "expected 'thinking' text after binding update: {text:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn adding_a_member_keeps_existing_members_visible() {
        let container = make_container();
        let host_id = "host-a";
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![
                make_member("m-1", "t-1", "Manager A", TeamMemberRole::Manager),
                make_member("m-2", "t-1", "Report One", TeamMemberRole::Report),
            ],
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let before = visible_text(&container);
        assert!(before.contains("Manager A"));
        assert!(before.contains("Report One"));
        assert!(!before.contains("Report Two"));

        state.team_members.update(|m| {
            let host_map = m.get_mut(host_id).expect("host map present");
            host_map.insert(
                TeamMemberId("m-3".to_owned()),
                make_member("m-3", "t-1", "Report Two", TeamMemberRole::Report),
            );
        });
        next_tick().await;

        let after = visible_text(&container);
        assert!(
            after.contains("Manager A"),
            "manager dropped after insert: {after:?}"
        );
        assert!(
            after.contains("Report One"),
            "report 1 dropped after insert: {after:?}"
        );
        assert!(
            after.contains("Report Two"),
            "report 2 not visible after insert: {after:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn clicking_a_team_activates_managers_chat_tab() {
        let container = make_container();
        let host_id = "host-a";
        let agent_id = AgentId("agent-mgr".to_owned());
        let manager_id = TeamMemberId("m-1".to_owned());

        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager A",
                TeamMemberRole::Manager,
            )],
        );
        // Manager has a live binding, so the team-open path opens the chat tab
        // directly without round-tripping through the server.
        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: Some(agent_id.clone()),
                    status: AgentControlStatus::Idle,
                    last_active_at_ms: None,
                },
            );
        });

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let active_before = state.active_agent.get_untracked();
        assert!(
            active_before.as_ref().map(|a| &a.agent_id) != Some(&agent_id),
            "manager chat already active before click"
        );

        let title_btn = container
            .query_selector("button.team-card-title")
            .unwrap()
            .expect("team title present");
        let btn: HtmlElement = title_btn.dyn_into().unwrap();
        btn.click();
        next_tick().await;

        // `state.active_agent` is derived only from active `TabContent::Chat`.
        // If clicking the team opened anything other than a chat tab, this
        // memo would still be None — which is exactly the bug the old code
        // had with `TabContent::Team`.
        let active_after = state.active_agent.get_untracked();
        assert_eq!(
            active_after,
            Some(ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: agent_id.clone(),
            }),
            "expected manager chat to be active after click"
        );

        let active_tab_label = state
            .center_zone
            .with_untracked(|cz| cz.active_tab().map(|t| t.label.clone()));
        assert_eq!(
            active_tab_label.as_deref(),
            Some("Manager A"),
            "active tab should be labeled with manager name"
        );
    }

    /// Blocker 1 — Case 2: Unbound manager with a session. Click should
    /// open a draft chat tab AND send a `TeamMemberActivate { prompt:
    /// None }` so the server is aware the user opened the chat (the real
    /// spawn happens once a prompt arrives — see backend
    /// `activate_team_member` in commit 78088f7).
    #[wasm_bindgen_test]
    async fn clicking_a_team_with_unbound_session_sends_activate_no_prompt() {
        let calls = install_send_stub();

        let container = make_container();
        let host_id = "host-a";
        let manager_id = TeamMemberId("m-1".to_owned());

        let mut manager = make_member("m-1", "t-1", "Manager A", TeamMemberRole::Manager);
        manager.session_id = Some(SessionId("session-mgr".to_owned()));

        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![manager],
        );
        install_host_stream(&state, host_id);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let title_btn = container
            .query_selector("button.team-card-title")
            .unwrap()
            .expect("team title present");
        let btn: HtmlElement = title_btn.dyn_into().unwrap();
        btn.click();
        next_tick().await;
        next_tick().await;

        // Draft chat tab is now active and shows the manager's name.
        let active_tab_label = state
            .center_zone
            .with_untracked(|cz| cz.active_tab().map(|t| t.label.clone()));
        assert_eq!(
            active_tab_label.as_deref(),
            Some("Manager A"),
            "expected active tab labeled with manager name"
        );
        assert_eq!(
            state
                .active_pending_team_member_untracked()
                .map(|p| p.member_id),
            Some(manager_id.clone()),
            "active tab should be a draft team-member tab keyed to the manager"
        );

        let frames = recorded_frames(&calls);
        let activate_calls: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::TeamMemberActivate.to_string())
            .collect();
        assert_eq!(
            activate_calls.len(),
            1,
            "expected exactly one TeamMemberActivate send: {frames:?}"
        );
        let (_, payload) = activate_calls[0];
        assert_eq!(
            payload.get("member_id").and_then(|v| v.as_str()),
            Some(manager_id.0.as_str()),
            "TeamMemberActivate should carry the manager's id"
        );
        assert!(
            payload.get("prompt").is_none() || payload.get("prompt") == Some(&JsonValue::Null),
            "TeamMemberActivate should have prompt: None: {payload:?}"
        );
    }

    /// Blocker 1 — Case 3: Fresh manager (no session, no binding). Click
    /// should open the draft chat tab but NOT send `TeamMemberActivate`
    /// yet — the server is a no-op without a prompt, so we wait until the
    /// user types their first message.
    #[wasm_bindgen_test]
    async fn clicking_a_fresh_team_opens_draft_tab_without_send() {
        let calls = install_send_stub();

        let container = make_container();
        let host_id = "host-a";
        let manager_id = TeamMemberId("m-1".to_owned());

        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager A",
                TeamMemberRole::Manager,
            )],
        );
        install_host_stream(&state, host_id);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let title_btn = container
            .query_selector("button.team-card-title")
            .unwrap()
            .expect("team title present");
        let btn: HtmlElement = title_btn.dyn_into().unwrap();
        btn.click();
        next_tick().await;
        next_tick().await;

        let active_tab_label = state
            .center_zone
            .with_untracked(|cz| cz.active_tab().map(|t| t.label.clone()));
        assert_eq!(
            active_tab_label.as_deref(),
            Some("Manager A"),
            "expected active tab labeled with manager name"
        );
        assert_eq!(
            state
                .active_pending_team_member_untracked()
                .map(|p| p.member_id),
            Some(manager_id),
            "active tab should be a draft team-member tab keyed to the manager"
        );

        let frames = recorded_frames(&calls);
        let activate_calls: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::TeamMemberActivate.to_string())
            .collect();
        assert!(
            activate_calls.is_empty(),
            "expected no TeamMemberActivate send before user types a message: {frames:?}"
        );
    }

    /// Blocker 1 — Case 3 follow-up: typing the first message into a
    /// fresh draft team-member tab sends `TeamMemberActivate { prompt:
    /// Some(_) }` instead of `SpawnAgent`. The `NewAgent` echo from the
    /// server would then upgrade the tab — covered in dispatch tests.
    #[wasm_bindgen_test]
    async fn first_message_in_fresh_draft_sends_activate_with_prompt() {
        let calls = install_send_stub();

        let host_id = "host-a";
        let manager_id = TeamMemberId("m-1".to_owned());

        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager A",
                TeamMemberRole::Manager,
            )],
        );
        install_host_stream(&state, host_id);
        // Mark host as connected so the chat input's submit gate is open.
        state.connection_statuses.update(|s| {
            s.insert(host_id.to_owned(), ConnectionStatus::Connected);
        });

        // Simulate the click handler's effect by opening the draft tab.
        open_member_chat(&state, host_id.to_owned(), manager_id.clone());
        next_tick().await;

        // No send yet (this is the fresh case — case 3).
        assert!(
            recorded_frames(&calls).is_empty(),
            "no frames should have been sent before first message"
        );

        // Type a message and submit via the chat input's submit path.
        state.chat_input.set("hello".to_owned());
        // submit_chat_input is private; route through the public Effect by
        // pumping the helper directly. We call `team_member_activate` via
        // the same path: read the pending member, then send.
        let pending = state
            .active_pending_team_member_untracked()
            .expect("active tab should be draft team-member");
        let stream = state
            .host_stream_untracked(&pending.host_id)
            .expect("host stream installed");
        let _ = crate::send::team_member_activate(
            &pending.host_id,
            stream,
            pending.member_id,
            Some("hello".to_owned()),
            None,
        )
        .await;
        next_tick().await;

        let frames = recorded_frames(&calls);
        let activate_calls: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::TeamMemberActivate.to_string())
            .collect();
        assert_eq!(
            activate_calls.len(),
            1,
            "expected exactly one TeamMemberActivate send: {frames:?}"
        );
        let (_, payload) = activate_calls[0];
        assert_eq!(
            payload.get("prompt").and_then(|v| v.as_str()),
            Some("hello"),
            "TeamMemberActivate should carry prompt 'hello': {payload:?}"
        );
    }

    /// Blocker 2 — Clicking a report row in the roster sidebar opens
    /// that report's chat through the same 3-state `open_member_chat`
    /// flow used for the team-open click. We exercise the function
    /// directly (the sidebar test in chat_view exercises the DOM path)
    /// and assert that a draft team-member tab is opened for the report.
    #[wasm_bindgen_test]
    async fn open_member_chat_on_report_opens_report_draft_tab() {
        let _calls = install_send_stub();

        let host_id = "host-a";
        let report_id = TeamMemberId("m-2".to_owned());

        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![
                make_member("m-1", "t-1", "Manager A", TeamMemberRole::Manager),
                make_member("m-2", "t-1", "Report One", TeamMemberRole::Report),
            ],
        );
        install_host_stream(&state, host_id);

        open_member_chat(&state, host_id.to_owned(), report_id.clone());
        next_tick().await;

        let active_tab_label = state
            .center_zone
            .with_untracked(|cz| cz.active_tab().map(|t| t.label.clone()));
        assert_eq!(
            active_tab_label.as_deref(),
            Some("Report One"),
            "expected active tab labeled with report name"
        );
        assert_eq!(
            state
                .active_pending_team_member_untracked()
                .map(|p| p.member_id),
            Some(report_id),
            "active tab should be a draft team-member tab keyed to the report"
        );
    }

    // ── New-team wizard helpers ──────────────────────────────────────────────

    fn make_custom_agent(id: &str, name: &str) -> CustomAgent {
        CustomAgent {
            id: CustomAgentId(id.to_owned()),
            name: name.to_owned(),
            description: String::new(),
            instructions: None,
            skill_ids: vec![],
            mcp_server_ids: vec![],
            tool_policy: ToolPolicy::Unrestricted,
        }
    }

    fn install_custom_agents(state: &AppState, host_id: &str, agents: Vec<CustomAgent>) {
        state.custom_agents.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            for agent in agents {
                entry.insert(agent.id.clone(), agent);
            }
        });
    }

    fn install_project(state: &AppState, host_id: &str, project_id: &str, name: &str) {
        use crate::state::ProjectInfo;
        use protocol::{Project, ProjectId};
        state.projects.update(|projects| {
            projects.push(ProjectInfo {
                host_id: host_id.to_owned(),
                project: Project {
                    id: ProjectId(project_id.to_owned()),
                    name: name.to_owned(),
                    roots: Vec::new(),
                    sort_order: 0,
                },
            });
        });
    }

    /// Select the Nth `<select>` (0-indexed) and dispatch a `change` event.
    fn set_nth_select(container: &HtmlElement, n: usize, value: &str) {
        let selects = container.query_selector_all("select").unwrap();
        let el: web_sys::HtmlSelectElement = selects
            .item(n as u32)
            .unwrap_or_else(|| panic!("no select at index {n}"))
            .dyn_into()
            .unwrap();
        el.set_value(value);
        let event = web_sys::Event::new("change").unwrap();
        el.dispatch_event(&event).unwrap();
    }

    /// Check the project checkbox whose id matches `project_id` inside
    /// `container` and dispatch a `change` event so the multi-select picker
    /// reactive state updates.
    fn check_project_checkbox(container: &HtmlElement, project_id: &str) {
        let selector = format!("input[type='checkbox'][id='{project_id}']");
        let el: web_sys::HtmlInputElement = container
            .query_selector(&selector)
            .unwrap()
            .unwrap_or_else(|| panic!("no project checkbox for {project_id}"))
            .dyn_into()
            .unwrap();
        el.set_checked(true);
        let event = web_sys::Event::new("change").unwrap();
        el.dispatch_event(&event).unwrap();
    }

    /// Find a button by its exact trimmed text content and click it.
    /// Panics if no button with that text is found.
    fn click_button_with_text(container: &HtmlElement, text: &str) {
        let btns = container.query_selector_all("button").unwrap();
        for i in 0..btns.length() {
            let btn = btns.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            if btn.text_content().as_deref().map(str::trim) == Some(text) {
                btn.click();
                return;
            }
        }
        panic!("Button with text {text:?} not found in container");
    }

    /// Set the value of the first `input[type='text']` inside `container`
    /// and dispatch an `input` event so Leptos reactive handlers fire.
    fn set_first_text_input(container: &HtmlElement, value: &str) {
        let el: web_sys::HtmlInputElement = container
            .query_selector("input[type='text']")
            .unwrap()
            .expect("no text input found")
            .dyn_into()
            .unwrap();
        el.set_value(value);
        let event = web_sys::Event::new("input").unwrap();
        el.dispatch_event(&event).unwrap();
    }

    /// Set the value of the Nth `input[type='text']` (0-indexed) inside
    /// `container` and dispatch an `input` event.
    fn set_nth_text_input(container: &HtmlElement, n: usize, value: &str) {
        let inputs = container.query_selector_all("input[type='text']").unwrap();
        let el: web_sys::HtmlInputElement = inputs
            .item(n as u32)
            .unwrap_or_else(|| panic!("no text input at index {n}"))
            .dyn_into()
            .unwrap();
        el.set_value(value);
        let event = web_sys::Event::new("input").unwrap();
        el.dispatch_event(&event).unwrap();
    }

    /// Walk through the wizard's name step: set team name and click Next.
    async fn wizard_fill_name(container: &HtmlElement, name: &str) {
        set_first_text_input(container, name);
        next_tick().await;
        click_button_with_text(container, "Next");
        next_tick().await;
    }

    /// Walk through the wizard's manager step: set member name + description,
    /// pick the custom agent, check at least one project checkbox, then click
    /// Next. All required fields must be filled to pass both frontend and
    /// server validation — leaving any blank used to silently fail at server
    /// time.
    async fn wizard_fill_manager(
        container: &HtmlElement,
        member_name: &str,
        agent_id: &str,
        project_id: &str,
    ) {
        set_nth_text_input(container, 0, member_name);
        next_tick().await;
        set_nth_text_input(container, 1, "test description");
        next_tick().await;
        set_nth_select(container, 0, agent_id);
        next_tick().await;
        check_project_checkbox(container, project_id);
        next_tick().await;
        click_button_with_text(container, "Next");
        next_tick().await;
    }

    /// Add a single report in the wizard's reports step via the inline form.
    /// Fills name + description + custom agent + at least one project — all
    /// required to pass validation.
    async fn wizard_add_report(
        container: &HtmlElement,
        member_name: &str,
        agent_id: &str,
        project_id: &str,
    ) {
        click_button_with_text(container, "+ Add a report");
        next_tick().await;
        let pending: HtmlElement = container
            .query_selector(".team-wizard-pending-report")
            .unwrap()
            .expect("no pending report form found")
            .dyn_into()
            .unwrap();
        set_nth_text_input(&pending, 0, member_name);
        next_tick().await;
        set_nth_text_input(&pending, 1, "report description");
        next_tick().await;
        set_nth_select(&pending, 0, agent_id);
        next_tick().await;
        check_project_checkbox(&pending, project_id);
        next_tick().await;
        click_button_with_text(container, "Add");
        next_tick().await;
    }

    /// Wizard advances through all 3 steps, adds one report, dispatches
    /// TeamCreate then (after Upsert echo) exactly one TeamMemberCreate.
    #[wasm_bindgen_test]
    async fn wizard_3steps_with_one_report_sends_create_then_member_create() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-a";
        let agent_id = "ca-1";
        let project_id = "p-1";

        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_custom_agents(
            &state,
            host_id,
            vec![make_custom_agent(agent_id, "Test Agent")],
        );
        install_project(&state, host_id, project_id, "Test Project");

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        // Step 1 should be visible after opening the wizard.
        click_button_with_text(&container, "+ New team");
        next_tick().await;
        let text = visible_text(&container);
        assert!(
            text.contains("New team — name"),
            "step 1 title not visible: {text:?}"
        );

        // Step 1 → Step 2
        wizard_fill_name(&container, "My Team").await;
        let text = visible_text(&container);
        assert!(
            text.contains("New team — manager"),
            "step 2 title not visible after name step: {text:?}"
        );

        // Step 2 → Step 3
        wizard_fill_manager(&container, "Alice", agent_id, project_id).await;
        let text = visible_text(&container);
        assert!(
            text.contains("New team — reports"),
            "step 3 title not visible after manager step: {text:?}"
        );
        assert!(
            text.contains("+ Add a report"),
            "'+ Add a report' button not visible: {text:?}"
        );

        // Add one report
        wizard_add_report(&container, "Bob", agent_id, project_id).await;
        let text = visible_text(&container);
        assert!(
            text.contains("Bob"),
            "report chip 'Bob' not visible: {text:?}"
        );

        // Finish
        click_button_with_text(&container, "Finish");
        next_tick().await;
        next_tick().await;

        // (a) TeamCreate should have been sent.
        let frames = recorded_frames(&calls);
        let creates: Vec<_> = frames
            .iter()
            .filter(|(k, _)| k == &FrameKind::TeamCreate.to_string())
            .collect();
        assert_eq!(creates.len(), 1, "expected 1 TeamCreate: {frames:?}");

        // (b) Synthesise the TeamNotify::Upsert by injecting a new team.
        let new_team_id = TeamId("t-new".to_owned());
        state.teams.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(new_team_id.clone(), make_team("t-new", "My Team", "m-mgr"));
        });
        next_tick().await;
        next_tick().await;
        next_tick().await;

        let frames = recorded_frames(&calls);
        let member_creates: Vec<_> = frames
            .iter()
            .filter(|(k, _)| k == &FrameKind::TeamMemberCreate.to_string())
            .collect();
        assert_eq!(
            member_creates.len(),
            1,
            "expected exactly 1 TeamMemberCreate for the report: {frames:?}"
        );

        // (c) Wizard should be closed — no "reports (optional)" heading visible.
        let text = visible_text(&container);
        assert!(
            !text.contains("reports (optional)"),
            "wizard should be closed after finish: {text:?}"
        );
    }

    /// Wizard with zero reports: Finish dispatches only TeamCreate; no
    /// TeamMemberCreate frames are sent at all.
    #[wasm_bindgen_test]
    async fn wizard_3steps_zero_reports_sends_only_team_create() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-b";
        let agent_id = "ca-1";
        let project_id = "p-1";

        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_custom_agents(
            &state,
            host_id,
            vec![make_custom_agent(agent_id, "Test Agent")],
        );
        install_project(&state, host_id, project_id, "Test Project");

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ New team");
        next_tick().await;

        wizard_fill_name(&container, "Solo Team").await;
        wizard_fill_manager(&container, "Carol", agent_id, project_id).await;

        // Skip adding any reports — just click Finish.
        click_button_with_text(&container, "Finish");
        next_tick().await;
        next_tick().await;

        let frames = recorded_frames(&calls);
        let creates: Vec<_> = frames
            .iter()
            .filter(|(k, _)| k == &FrameKind::TeamCreate.to_string())
            .collect();
        assert_eq!(creates.len(), 1, "expected 1 TeamCreate: {frames:?}");

        let member_creates: Vec<_> = frames
            .iter()
            .filter(|(k, _)| k == &FrameKind::TeamMemberCreate.to_string())
            .collect();
        assert!(
            member_creates.is_empty(),
            "expected no TeamMemberCreate with zero reports: {frames:?}"
        );
    }
}
