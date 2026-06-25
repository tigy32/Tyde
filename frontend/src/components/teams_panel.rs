use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentControlStatus, AgentId, BackendKind, CustomAgent, CustomAgentId, ProjectId, SpawnCostHint,
    StreamPath, Team, TeamDraft, TeamDraftId, TeamDraftMember, TeamDraftMemberEdit,
    TeamDraftMemberId, TeamDraftShuffleScope, TeamId, TeamMember, TeamMemberBindingPayload,
    TeamMemberCreateSpec, TeamMemberId, TeamMemberPresetProfile, TeamMemberRole, TeamMemberState,
    TeamMemberUpdatePayload, TeamPersonalityPresetId, TeamPersonalityTrait, TeamRolePresetId,
    TeamTemplateId,
};

use crate::send::{
    team_delete, team_draft_add_report, team_draft_apply_template, team_draft_commit,
    team_draft_create, team_draft_discard, team_draft_remove_member, team_draft_replace_member,
    team_draft_set_member_profile, team_draft_set_name, team_draft_shuffle, team_member_create,
    team_member_delete, team_member_shuffle, team_member_update, team_set_manager,
};
use crate::state::{ActiveAgentRef, AppState, TabContent};

#[derive(Clone)]
pub(crate) struct MemberFormState {
    pub(crate) team_id: TeamId,
    pub(crate) editing_id: Option<TeamMemberId>,
    pub(crate) is_manager: bool,
    pub(crate) name: RwSignal<String>,
    pub(crate) description: RwSignal<String>,
    pub(crate) profile: RwSignal<Option<TeamMemberPresetProfile>>,
    pub(crate) custom_agent_id: RwSignal<Option<CustomAgentId>>,
    pub(crate) backend_kind: RwSignal<Option<BackendKind>>,
    pub(crate) cost_hint: RwSignal<Option<SpawnCostHint>>,
    pub(crate) project_ids: RwSignal<Vec<ProjectId>>,
}

impl MemberFormState {
    fn new_report(team_id: TeamId) -> Self {
        Self {
            team_id,
            editing_id: None,
            is_manager: false,
            name: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            profile: RwSignal::new(None),
            custom_agent_id: RwSignal::new(None),
            backend_kind: RwSignal::new(None),
            cost_hint: RwSignal::new(None),
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
            profile: RwSignal::new(member.profile.clone()),
            custom_agent_id: RwSignal::new(member.custom_agent_id.clone()),
            backend_kind: RwSignal::new(Some(member.backend_kind)),
            cost_hint: RwSignal::new(member.cost_hint),
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
    let custom_agent_id = form.custom_agent_id.get_untracked();
    let backend_kind = form
        .backend_kind
        .get_untracked()
        .ok_or_else(|| "Pick a backend.".to_string())?;
    let cost_hint = form.cost_hint.get_untracked();
    let project_ids = form.project_ids.get_untracked();
    if project_ids.is_empty() {
        return Err("Pick at least one project.".to_string());
    }
    Ok(TeamMemberCreateSpec {
        name,
        description,
        profile: form.profile.get_untracked(),
        custom_agent_id,
        backend_kind,
        cost_hint,
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
        profile: form.profile.get_untracked(),
        project_ids,
    })
}

type ActiveTeamSelection = Option<(String, TeamMemberId, TeamId)>;

#[component]
pub fn TeamsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();

    let new_team_open: RwSignal<bool> = RwSignal::new(false);
    let member_form: RwSignal<Option<MemberFormState>> = RwSignal::new(None);

    let active_team_state = state.clone();
    let active_team_selection: Memo<ActiveTeamSelection> = Memo::new(move |_| {
        if let Some(active) = active_team_state.active_agent.get() {
            let host_id = active.host_id.clone();
            let agent_id = active.agent_id.clone();
            return active_team_state.team_member_bindings.with(|bindings_map| {
                let member_id = bindings_map
                    .get(&host_id)?
                    .values()
                    .find(|binding| binding.current_agent_id.as_ref() == Some(&agent_id))
                    .map(|binding| binding.member_id.clone())?;
                active_team_state.team_members.with(|members_map| {
                    let member = members_map.get(&host_id)?.get(&member_id)?;
                    Some((host_id.clone(), member_id, member.team_id.clone()))
                })
            });
        }

        active_team_state.center_zone.with(|cz| {
            let pending = match &cz.active_tab()?.content {
                TabContent::Chat {
                    agent_ref: None,
                    pending_team_member: Some(pending),
                } => pending.clone(),
                _ => return None,
            };
            active_team_state.team_members.with(|members_map| {
                let member = members_map.get(&pending.host_id)?.get(&pending.member_id)?;
                Some((
                    pending.host_id.clone(),
                    pending.member_id.clone(),
                    member.team_id.clone(),
                ))
            })
        })
    });

    let teams_state = state.clone();
    // Pair each team with its host_id at the source, so downstream rows never
    // need to read the ambient `selected_host_id` to know which host they belong
    // to. That removes the bug where a user switches hosts while a team tab is
    // open and the row's actions read from the wrong host's signals.
    let active_team_for_sort = active_team_selection;
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
        let active = active_team_for_sort.get();
        teams.sort_by(|a, b| {
            let a_active = active.as_ref().is_some_and(|(active_host, _, team_id)| {
                active_host == &host_id && team_id == &a.id
            });
            let b_active = active.as_ref().is_some_and(|(active_host, _, team_id)| {
                active_host == &host_id && team_id == &b.id
            });
            b_active.cmp(&a_active).then(a.name.cmp(&b.name))
        });
        teams.into_iter().map(|t| (host_id.clone(), t)).collect()
    });

    let state_new = state.clone();
    let on_new_team = move |_| {
        if state_new.selected_host_id.get_untracked().is_none() {
            return;
        }
        new_team_open.set(true);
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
                                    active_team_selection=active_team_selection
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

            {move || new_team_open.get().then(|| view! {
                <NewTeamDialog on_close=Callback::new(move |_: ()| new_team_open.set(false)) />
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
    active_team_selection: Memo<ActiveTeamSelection>,
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
    let host_for_active = host_id.clone();
    let team_id_for_active = team_id.clone();
    let is_active_team: Memo<bool> = Memo::new(move |_| {
        active_team_selection
            .get()
            .as_ref()
            .is_some_and(|(active_host, _, active_team_id)| {
                active_host == &host_for_active && active_team_id == &team_id_for_active
            })
    });

    // Team-level Compact targets: mirror the server's accept/reject
    // rules so the button only enables when the server would accept the
    // `TeamCompact` frame. Returns `None` (button disabled) when:
    //   * any Active member is missing a binding entry (server treats
    //     this as an internal error),
    //   * any Active member's binding is not Idle (server rejects with
    //     conflict), regardless of whether it's bound to a live agent,
    //   * any Active+Idle+bound member is already mid-compaction,
    //   * any Active+Idle+bound member is missing from `state.agents`.
    // Active+Idle members with no `current_agent_id` are skipped — the
    // server accepts them too (nothing to compact). Members in any
    // non-Active state are skipped. Returns `None` if the resulting
    // target list is empty (nothing to compact).
    let host_for_team_targets = host_id.clone();
    let state_for_team_targets = state.clone();
    let members_for_team_targets = members;
    let team_compact_targets = move || -> Option<Vec<(AgentId, StreamPath)>> {
        let bindings = state_for_team_targets
            .team_member_bindings
            .with(|m| m.get(&host_for_team_targets).cloned())?;
        let compacting = state_for_team_targets
            .compaction_in_progress
            .with(|m| m.keys().cloned().collect::<std::collections::HashSet<_>>());
        let agents_snapshot = state_for_team_targets.agents.get();
        let mut targets: Vec<(AgentId, StreamPath)> = Vec::new();
        for member in members_for_team_targets.get() {
            if !matches!(member.state, TeamMemberState::Active) {
                continue;
            }
            let binding = bindings.get(&member.id)?;
            if !matches!(binding.status, protocol::AgentControlStatus::Idle) {
                return None;
            }
            let Some(agent_id) = binding.current_agent_id.clone() else {
                continue;
            };
            if compacting.contains(&agent_id) {
                return None;
            }
            let stream = agents_snapshot
                .iter()
                .find(|a| a.host_id == host_for_team_targets && a.agent_id == agent_id)
                .map(|a| a.instance_stream.clone())?;
            targets.push((agent_id, stream));
        }
        if targets.is_empty() {
            return None;
        }
        Some(targets)
    };

    let host_for_team_compact_gate = host_id.clone();
    let state_for_team_compact_gate = state.clone();
    let team_compact_targets_for_gate = team_compact_targets.clone();
    let can_compact_team = move || {
        if !matches!(
            state_for_team_compact_gate.connection_status_for_host(&host_for_team_compact_gate),
            crate::state::ConnectionStatus::Connected
        ) {
            return false;
        }
        team_compact_targets_for_gate().is_some()
    };

    let host_for_team_compact_click = host_id.clone();
    let state_for_team_compact_click = state.clone();
    let team_name_for_compact = team_record;
    let team_id_for_compact_click = team_id.clone();
    let team_compact_targets_for_click = team_compact_targets;
    let on_team_compact_click = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let Some(targets) = team_compact_targets_for_click() else {
            return;
        };
        let host_id = host_for_team_compact_click.clone();
        let state = state_for_team_compact_click.clone();
        let team_id = team_id_for_compact_click.clone();
        let team_label = team_name_for_compact
            .get_untracked()
            .map(|t| t.name)
            .unwrap_or_default();
        let count = targets.len();
        let plural = if count == 1 { "agent" } else { "agents" };
        let message = format!(
            "Compact context for every member of \"{team_label}\"?\n\nEach of the {count} bound {plural} will write a summary of context worth keeping and a fresh replacement will start from that summary. The original sessions are closed and kept in Sessions as read-only records — you can view them, but they can't be resumed."
        );
        spawn_local(async move {
            if !crate::bridge::confirm_dialog("Compact context", &message).await {
                return;
            }
            // Optimistically flag every targeted agent in-flight so the
            // per-member Compact icons and the team button itself
            // re-gate to disabled until per-agent AgentCompactNotify
            // events settle. Mirrors the per-member compact path's
            // double-fire defense.
            for (agent_id, _) in &targets {
                state.mark_compaction_started(&host_id, agent_id.clone());
            }
            let Some(host_stream) = state.host_stream_untracked(&host_id) else {
                log::error!("team compact: no host stream for {host_id}");
                for (agent_id, _) in targets {
                    state.finish_compaction_failure(agent_id, "no host stream".to_string());
                }
                return;
            };
            if let Err(e) = crate::send::team_compact(&host_id, host_stream, team_id).await {
                log::error!("team compact: failed to send TeamCompact: {e}");
                for (agent_id, _) in targets {
                    state.finish_compaction_failure(agent_id, e.clone());
                }
            }
        });
    };

    let host_for_rows = host_id.clone();
    view! {
        <div
            class=move || {
                if is_active_team.get() {
                    "team-card team-card-active"
                } else {
                    "team-card"
                }
            }
            data-team-id=team_id.0.clone()
        >
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
                    {move || is_active_team.get().then(|| view! {
                        <span class="team-card-active-badge">"Active"</span>
                    })}
                </button>
                <div class="team-card-actions">
                    <button
                        class="filter-toggle"
                        type="button"
                        on:click=move |_| on_add_report.run(())
                    >
                        "+ Report"
                    </button>
                    {move || {
                        let enabled = can_compact_team();
                        view! {
                            <button
                                class="filter-toggle team-card-compact"
                                type="button"
                                title=if enabled {
                                    "Compact context for every bound team member"
                                } else {
                                    "Compact context (available when every bound member is idle)"
                                }
                                aria-label="Compact context"
                                disabled=!enabled
                                on:click=on_team_compact_click.clone()
                            >
                                "Compact context"
                            </button>
                        }
                    }}
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
                                active_team_selection=active_team_selection
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
    active_team_selection: Memo<ActiveTeamSelection>,
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
    let binding: Memo<Option<TeamMemberBindingPayload>> = Memo::new(move |_| {
        state_for_binding.team_member_bindings.with(|map| {
            map.get(&host_for_binding)
                .and_then(|m| m.get(&mid_for_binding))
                .cloned()
        })
    });
    let binding_status = move || binding.get().map(|binding| binding.status);
    let last_active_label = move || {
        binding
            .get()
            .and_then(|binding| binding.last_active_at_ms)
            .map(|_| "last active recorded".to_owned())
    };

    let host_for_agent = host_id.clone();
    let custom_agent_state = state.clone();
    let agent_profile_label = move || -> Option<String> {
        let m = member.get()?;
        let custom_agent = match m.custom_agent_id.as_ref() {
            Some(custom_agent_id) => custom_agent_state.custom_agents.with(|map| {
                map.get(&host_for_agent)
                    .and_then(|m2| m2.get(custom_agent_id).map(|c| c.name.clone()))
                    .unwrap_or_else(|| custom_agent_id.0.clone())
            }),
            None => "Default agent".to_owned(),
        };
        Some(format!(
            "{custom_agent} · {}{}",
            backend_kind_label(m.backend_kind),
            cost_hint_suffix(m.cost_hint)
        ))
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
    let mid_for_delete = member_id.clone();
    let mid_for_promote = member_id.clone();

    let on_click = move |_: web_sys::MouseEvent| {
        on_open.run(mid_for_open.clone());
    };

    let on_edit_click = {
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            let Some(m) = member.get_untracked() else {
                return;
            };
            let is_manager = matches!(m.role, TeamMemberRole::Manager);
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

    // Compact/Rotate action on a team member operates on the live bound
    // agent. Gated on: there *is* a live binding for this member, that
    // binding is `Idle`, the binding's agent isn't already mid-
    // compaction, and the host is connected. Hidden otherwise so the
    // user never sees an enabled button they can't usefully press.
    let host_for_compact = host_id.clone();
    let binding_for_compact = binding;
    let state_for_compact_gate = state.clone();
    let can_compact = move || {
        let Some(binding) = binding_for_compact.get() else {
            return false;
        };
        if binding.current_agent_id.is_none() {
            return false;
        }
        if !matches!(binding.status, protocol::AgentControlStatus::Idle) {
            return false;
        }
        let Some(agent_id) = binding.current_agent_id else {
            return false;
        };
        if state_for_compact_gate
            .compaction_in_progress
            .with(|map| map.contains_key(&agent_id))
        {
            return false;
        }
        matches!(
            state_for_compact_gate.connection_status_for_host(&host_for_compact),
            crate::state::ConnectionStatus::Connected
        )
    };
    let host_for_compact_click = host_id.clone();
    let binding_for_compact_click = binding;
    let state_for_compact_click = state.clone();
    let on_compact_click = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let Some(binding) = binding_for_compact_click.get_untracked() else {
            return;
        };
        let Some(agent_id) = binding.current_agent_id else {
            return;
        };
        let host_id = host_for_compact_click.clone();
        let state = state_for_compact_click.clone();
        let agent_stream = state.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == host_id && a.agent_id == agent_id)
                .map(|a| a.instance_stream.clone())
        });
        let Some(agent_stream) = agent_stream else {
            log::error!(
                "team-member compact: bound agent {} not found on host {host_id}",
                agent_id.0
            );
            return;
        };
        // The server marks the predecessor session non-resumable as
        // part of the compaction protocol, so don't promise the user
        // they can pick it back up. The summary stays in Sessions as
        // a read-only record of what was kept.
        let message =
            "Compact agent for this team member?\n\nThe agent will write a summary of context worth keeping and a fresh replacement will start from that summary. The original session is closed and kept in Sessions as a read-only record — you can view it, but it can't be resumed.".to_string();
        spawn_local(async move {
            if !crate::bridge::confirm_dialog("Compact agent", &message).await {
                return;
            }
            state.mark_compaction_started(&host_id, agent_id.clone());
            if let Err(e) = crate::send::compact_agent(&host_id, agent_stream).await {
                log::error!("team-member compact: failed to send AgentCompact: {e}");
                state.finish_compaction_failure(agent_id, e);
            }
        });
    };

    let can_promote = move || matches!(member.get().map(|m| m.role), Some(TeamMemberRole::Report));
    let host_for_active = host_id.clone();
    let member_id_for_active = member_id.clone();
    let is_active_member: Memo<bool> = Memo::new(move |_| {
        active_team_selection
            .get()
            .as_ref()
            .is_some_and(|(active_host, active_member_id, _)| {
                active_host == &host_for_active && active_member_id == &member_id_for_active
            })
    });

    view! {
        <div
            class=move || {
                if is_active_member.get() {
                    "team-member-row team-member-row-active"
                } else {
                    "team-member-row"
                }
            }
            role="button"
            tabindex="0"
            aria-current=move || if is_active_member.get() { "true" } else { "false" }
            on:click=on_click
        >
            <div class="team-member-main">
                <div class="team-member-name-row">
                    {move || binding_status().map(|status| {
                        let label = agent_control_status_label(status);
                        view! {
                            <span
                                class=format!(
                                    "team-member-status-dot {}",
                                    agent_control_status_dot_class(status)
                                )
                                role="img"
                                title=label
                                aria-label=label
                            />
                        }
                    })}
                    <span class="team-member-name">
                        {move || member.get().map(|m| m.name).unwrap_or_default()}
                    </span>
                    {move || is_active_member.get().then(|| view! {
                        <span class="team-member-active-badge">"Active"</span>
                    })}
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
                </div>
                <div class="team-member-meta">
                    {move || agent_profile_label().map(|s| view! {
                        <span class="team-member-custom-agent">{s}</span>
                    })}
                    {move || {
                        let s = project_labels();
                        (!s.is_empty()).then(|| view! {
                            <span class="team-member-projects-summary">{s}</span>
                        })
                    }}
                    {move || last_active_label().map(|label| view! {
                        <span class="team-member-last-active">{label}</span>
                    })}
                </div>
            </div>
            <div class="team-member-actions">
                {move || can_promote().then(|| view! {
                    <button
                        class="team-member-icon-btn team-member-icon-btn-promote"
                        type="button"
                        title="Set as manager"
                        aria-label="Set as manager"
                        on:click=on_promote_click.clone()
                    >"\u{2605}"</button>
                })}
                {move || can_compact().then(|| view! {
                    <button
                        class="team-member-icon-btn team-member-icon-btn-compact"
                        type="button"
                        title="Compact agent"
                        aria-label="Compact agent"
                        on:click=on_compact_click.clone()
                    >"\u{27F2}"</button>
                })}
                <button
                    class="team-member-icon-btn"
                    type="button"
                    title="Edit member"
                    aria-label="Edit member"
                    on:click=on_edit_click
                >"\u{270E}"</button>
                <button
                    class="team-member-icon-btn team-member-icon-btn-danger"
                    type="button"
                    title="Delete member"
                    aria-label="Delete member"
                    on:click=on_delete_click
                >"\u{1F5D1}"</button>
            </div>
        </div>
    }
}

#[component]
fn NewTeamDialog(on_close: Callback<()>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let error_sig: RwSignal<Option<String>> = RwSignal::new(None);
    let submitting: RwSignal<bool> = RwSignal::new(false);

    let draft_state = state.clone();
    let current_draft: Memo<Option<(String, TeamDraft)>> = Memo::new(move |_| {
        let host_id = draft_state.selected_host_id.get()?;
        draft_state.team_drafts.with(|drafts| {
            let mut values = drafts.get(&host_id)?.values().cloned().collect::<Vec<_>>();
            values.sort_by(|a, b| {
                a.created_at_ms
                    .cmp(&b.created_at_ms)
                    .then(a.id.0.cmp(&b.id.0))
            });
            values
                .into_iter()
                .next()
                .map(|draft| (host_id.clone(), draft))
        })
    });
    let current_draft_key: Memo<Option<(String, TeamDraftId)>> = Memo::new(move |_| {
        current_draft.with(|draft| {
            draft
                .as_ref()
                .map(|(host_id, draft)| (host_id.clone(), draft.id.clone()))
        })
    });

    let catalog_state = state.clone();
    let catalog = Memo::new(move |_| {
        let host_id = catalog_state.selected_host_id.get()?;
        catalog_state
            .team_preset_catalogs
            .with(|catalogs| catalogs.get(&host_id).cloned())
    });

    // Projects available on the host that hosts the open draft. Sorted by
    // display name so the upfront picker is stable.
    let projects_state = state.clone();
    let host_projects: Memo<Vec<protocol::Project>> = Memo::new(move |_| {
        let Some(host_id) = projects_state.selected_host_id.get() else {
            return Vec::new();
        };
        let mut projects: Vec<protocol::Project> = projects_state
            .projects
            .get()
            .into_iter()
            .filter(|p| p.host_id == host_id)
            .map(|p| p.project)
            .collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        projects
    });

    // Default backend declared in HostSettings for the draft's host. Used to
    // auto-fill each member's backend the moment the draft appears, so the
    // user only has to override per-member when they really want to.
    let backend_state = state.clone();
    let default_backend: Memo<Option<BackendKind>> = Memo::new(move |_| {
        let host_id = backend_state.selected_host_id.get()?;
        backend_state
            .host_settings_by_host
            .with(|map| map.get(&host_id).and_then(|s| s.default_backend))
    });

    // One project applies to every member of the new team. Seeded from the
    // currently-active project when that project lives on the draft's host;
    // otherwise from the first available project. The user can still
    // override.
    let wizard_project: RwSignal<Option<ProjectId>> = RwSignal::new(None);
    let project_seed_state = state.clone();
    Effect::new(move |_| {
        if current_draft_key.get().is_none() {
            return;
        }
        if wizard_project.get_untracked().is_some() {
            return;
        }
        let Some(host_id) = project_seed_state.selected_host_id.get() else {
            return;
        };
        let available = host_projects.get();
        let active_match = project_seed_state
            .active_project
            .get()
            .filter(|active| active.host_id == host_id)
            .and_then(|active| {
                available
                    .iter()
                    .find(|p| p.id == active.project_id)
                    .map(|p| p.id.clone())
            });
        let seed = active_match.or_else(|| available.first().map(|p| p.id.clone()));
        if let Some(id) = seed {
            wizard_project.set(Some(id));
        }
    });

    let command_error_state = state.clone();
    let command_error = move || {
        let host_id = command_error_state.selected_host_id.get()?;
        command_error_state
            .command_errors_by_host
            .with(|errors| errors.get(&host_id).cloned())
    };

    let close_when_committed = on_close;
    let command_error_for_effect = command_error_state.clone();
    Effect::new(move |_| {
        if !submitting.get() {
            return;
        }
        if current_draft_key.get().is_none() {
            // Successful commit: the host's draft was deleted, so the
            // dialog can close.
            submitting.set(false);
            close_when_committed.run(());
            return;
        }
        // Server rejected the commit; the draft is preserved on the host
        // so the user can fix it. Re-enable retry/discard. We probe the
        // host's command_errors signal reactively here so a new error
        // (e.g. validation failure from the registry) clears the
        // submitting flag without inventing a silent fallback.
        let Some(host_id) = command_error_for_effect.selected_host_id.get() else {
            return;
        };
        if command_error_for_effect
            .command_errors_by_host
            .with(|errors| errors.get(&host_id).is_some())
        {
            submitting.set(false);
        }
    });

    let send_create: Callback<Option<TeamTemplateId>> = {
        let state = state.clone();
        Callback::new(move |template_id: Option<TeamTemplateId>| {
            let Some(host_id) = state.selected_host_id.get_untracked() else {
                error_sig.set(Some("No host selected.".to_string()));
                return;
            };
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                error_sig.set(Some("Host is not connected.".to_string()));
                return;
            };
            error_sig.set(None);
            spawn_local(async move {
                if let Err(error) = team_draft_create(&host_id, stream, template_id).await {
                    log::error!("team_draft_create failed: {error}");
                }
            });
        })
    };

    let send_apply_template: Callback<(TeamDraftId, TeamTemplateId)> = {
        let state = state.clone();
        Callback::new(
            move |(draft_id, template_id): (TeamDraftId, TeamTemplateId)| {
                let Some(host_id) = state.selected_host_id.get_untracked() else {
                    error_sig.set(Some("No host selected.".to_string()));
                    return;
                };
                let Some(stream) = state.host_stream_untracked(&host_id) else {
                    error_sig.set(Some("Host is not connected.".to_string()));
                    return;
                };
                error_sig.set(None);
                spawn_local(async move {
                    if let Err(error) =
                        team_draft_apply_template(&host_id, stream, draft_id, template_id).await
                    {
                        log::error!("team_draft_apply_template failed: {error}");
                    }
                });
            },
        )
    };

    let send_commit: Callback<TeamDraftId> = {
        let state = state.clone();
        Callback::new(move |draft_id: TeamDraftId| {
            let Some(host_id) = state.selected_host_id.get_untracked() else {
                error_sig.set(Some("No host selected.".to_string()));
                return;
            };
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                error_sig.set(Some("Host is not connected.".to_string()));
                return;
            };
            error_sig.set(None);
            // Clear any prior host-level command error so the Effect can
            // detect a *new* error from this commit and re-enable
            // retry/discard. Without this, a previous error would keep
            // the dialog enabled indefinitely or the new error would
            // never be observed as a transition.
            let host_id_for_clear = host_id.clone();
            state.command_errors_by_host.update(|errors| {
                errors.remove(&host_id_for_clear);
            });
            submitting.set(true);
            spawn_local(async move {
                if let Err(error) = team_draft_commit(&host_id, stream, draft_id).await {
                    log::error!("team_draft_commit failed: {error}");
                    error_sig.set(Some(error));
                    submitting.set(false);
                }
            });
        })
    };

    let send_discard: Callback<TeamDraftId> = {
        let state = state.clone();
        Callback::new(move |draft_id: TeamDraftId| {
            let Some(host_id) = state.selected_host_id.get_untracked() else {
                error_sig.set(Some("No host selected.".to_string()));
                return;
            };
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                error_sig.set(Some("Host is not connected.".to_string()));
                return;
            };
            error_sig.set(None);
            spawn_local(async move {
                if let Err(error) = team_draft_discard(&host_id, stream, draft_id).await {
                    log::error!("team_draft_discard failed: {error}");
                    error_sig.set(Some(error));
                    return;
                }
                on_close.run(());
            });
        })
    };

    let send_name: Callback<(TeamDraftId, String)> = {
        let state = state.clone();
        Callback::new(move |(draft_id, name): (TeamDraftId, String)| {
            let Some(host_id) = state.selected_host_id.get_untracked() else {
                error_sig.set(Some("No host selected.".to_string()));
                return;
            };
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                error_sig.set(Some("Host is not connected.".to_string()));
                return;
            };
            spawn_local(async move {
                if let Err(error) = team_draft_set_name(&host_id, stream, draft_id, name).await {
                    log::error!("team_draft_set_name failed: {error}");
                }
            });
        })
    };

    let send_add_report: Callback<TeamDraftId> = {
        let state = state.clone();
        Callback::new(move |draft_id: TeamDraftId| {
            let Some(host_id) = state.selected_host_id.get_untracked() else {
                error_sig.set(Some("No host selected.".to_string()));
                return;
            };
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                error_sig.set(Some("Host is not connected.".to_string()));
                return;
            };
            spawn_local(async move {
                if let Err(error) = team_draft_add_report(&host_id, stream, draft_id).await {
                    log::error!("team_draft_add_report failed: {error}");
                }
            });
        })
    };

    let send_shuffle_all: Callback<TeamDraftId> = {
        let state = state.clone();
        Callback::new(move |draft_id: TeamDraftId| {
            let Some(host_id) = state.selected_host_id.get_untracked() else {
                error_sig.set(Some("No host selected.".to_string()));
                return;
            };
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                error_sig.set(Some("Host is not connected.".to_string()));
                return;
            };
            spawn_local(async move {
                if let Err(error) = team_draft_shuffle(
                    &host_id,
                    stream,
                    draft_id,
                    None,
                    TeamDraftShuffleScope::Member,
                )
                .await
                {
                    log::error!("team_draft_shuffle all failed: {error}");
                }
            });
        })
    };

    // The upfront Project picker is authoritative for every draft
    // member's `project_ids` in this wizard — per-member project
    // controls are intentionally hidden, so the picker is the only way
    // to set project membership. Every time the picker changes, sync
    // each member to `[selected]`; if the picker is cleared, clear each
    // member so server-side validation surfaces the missing project
    // rather than letting a stale project linger.
    //
    // Backend is filled from `HostSettings.default_backend` only when
    // the member's `backend_kind` is missing — never overwrites a
    // per-member override the user picked from the backend select.
    //
    // The replace echoes back through `team_drafts` so each predicate
    // flips false on the next run and there is no loop.
    let autofill_state = state.clone();
    Effect::new(move |_| {
        let Some((host_id, draft)) = current_draft.get() else {
            return;
        };
        let backend = default_backend.get();
        let project = wizard_project.get();
        let project_target: Vec<ProjectId> = project
            .as_ref()
            .cloned()
            .map(|id| vec![id])
            .unwrap_or_default();
        let Some(stream) = autofill_state.host_stream_untracked(&host_id) else {
            return;
        };
        let draft_id = draft.id.clone();
        for member in draft.members.iter() {
            let needs_backend = member.backend_kind.is_none() && backend.is_some();
            let needs_project_sync = member.project_ids != project_target;
            if !needs_backend && !needs_project_sync {
                continue;
            }
            let mut edit = TeamDraftMemberEdit {
                id: member.id.clone(),
                name: member.name.clone(),
                description: member.description.clone(),
                custom_agent_id: member.custom_agent_id.clone(),
                backend_kind: member.backend_kind,
                cost_hint: member.cost_hint,
                project_ids: member.project_ids.clone(),
            };
            if needs_backend {
                edit.backend_kind = backend;
            }
            if needs_project_sync {
                edit.project_ids = project_target.clone();
            }
            let host = host_id.clone();
            let stream = stream.clone();
            let draft = draft_id.clone();
            spawn_local(async move {
                if let Err(error) = team_draft_replace_member(&host, stream, draft, edit).await {
                    log::error!("team_draft_replace_member autofill failed: {error}");
                }
            });
        }
    });

    view! {
        <ModalOverlay on_close=on_close wide=true>
            <h3 class="settings-confirm-title">"New team"</h3>
            {move || match current_draft_key.get() {
                None => view! {
                    <div class="team-draft-start">
                        <p class="settings-form-hint">
                            "Start blank, generate a balanced team, or choose a server-owned template."
                        </p>
                        <div class="team-draft-template-actions">
                            <button
                                class="settings-btn settings-btn-primary team-draft-start-blank"
                                type="button"
                                on:click=move |_| send_create.run(None)
                            >"Start blank"</button>
                            {move || catalog.get().and_then(|catalog| {
                                catalog.team_templates.into_iter().find(|template| template.balanced)
                            }).map(|template| {
                                let template_id = template.id.clone();
                                view! {
                                    <button
                                        class="settings-btn team-draft-balanced"
                                        type="button"
                                        on:click=move |_| send_create.run(Some(template_id.clone()))
                                    >"Generate balanced team"</button>
                                }
                            })}
                        </div>
                        <div class="team-draft-template-list">
                            {move || catalog.get().map(|catalog| {
                                catalog.team_templates.into_iter().filter(|template| !template.balanced).map(|template| {
                                    let template_id = template.id.clone();
                                    let summary = template.summary.clone();
                                    view! {
                                        <button
                                            class="team-draft-template-card"
                                            type="button"
                                            on:click=move |_| send_create.run(Some(template_id.clone()))
                                        >
                                            <span class="team-draft-template-name">{template.name}</span>
                                            <span class="team-draft-template-summary">{summary}</span>
                                        </button>
                                    }
                                }).collect_view()
                            })}
                        </div>
                    </div>
                }.into_any(),
                Some((host_id, draft_id)) => {
                    let draft_id_for_name = draft_id.clone();
                    let draft_id_for_apply = draft_id.clone();
                    let draft_id_for_add = draft_id.clone();
                    let draft_id_for_shuffle = draft_id.clone();
                    let draft_id_for_commit = draft_id.clone();
                    let draft_id_for_discard = draft_id.clone();
                    let draft_id_for_rows = draft_id.clone();
                    let draft_id_attr = draft_id.0.clone();
                    view! {
                        <div class="team-draft-editor" data-draft-id=draft_id_attr>
                            <label class="settings-form-label">
                                <span>"Team name"</span>
                                <input
                                    class="settings-text-input team-draft-name"
                                    type="text"
                                    prop:value=move || current_draft.with(|c| {
                                        c.as_ref().map(|(_, d)| d.name.clone()).unwrap_or_default()
                                    })
                                    on:input=move |ev| {
                                        send_name.run((draft_id_for_name.clone(), event_target_value(&ev)));
                                    }
                                    spellcheck="false"
                                    autocapitalize="none"
                                    autocomplete="off"
                                />
                            </label>
                            <label class="settings-form-label">
                                <span>"Project"<span class="settings-form-hint">" (applies to every team member)"</span></span>
                                <select
                                    class="settings-text-input team-draft-project-select"
                                    on:change=move |ev| {
                                        let value = event_target_value(&ev);
                                        wizard_project.set(
                                            (!value.is_empty()).then_some(ProjectId(value)),
                                        );
                                    }
                                >
                                    <option
                                        value=""
                                        prop:selected=move || wizard_project.get().is_none()
                                    >"— select project —"</option>
                                    {move || host_projects.get().into_iter().map(|project| {
                                        let id_val = project.id.clone();
                                        let value = project.id.0.clone();
                                        let label = project.name.clone();
                                        view! {
                                            <option
                                                value=value
                                                prop:selected=move || wizard_project.get().as_ref() == Some(&id_val)
                                            >{label}</option>
                                        }
                                    }).collect_view()}
                                </select>
                                {move || host_projects.get().is_empty().then(|| view! {
                                    <span class="settings-form-hint">"No projects on this host. Create one before adding team members."</span>
                                })}
                            </label>
                            <div class="team-draft-template-actions">
                                {move || catalog.get().and_then(|catalog| {
                                    catalog.team_templates.into_iter().find(|template| template.balanced)
                                }).map(|template| {
                                    let template_id = template.id.clone();
                                    let draft_id = draft_id_for_apply.clone();
                                    view! {
                                        <button
                                            class="settings-btn team-draft-balanced"
                                            type="button"
                                            on:click=move |_| send_apply_template.run((draft_id.clone(), template_id.clone()))
                                        >"Regenerate balanced team"</button>
                                    }
                                })}
                                <button
                                    class="settings-btn team-draft-shuffle-all"
                                    type="button"
                                    on:click=move |_| send_shuffle_all.run(draft_id_for_shuffle.clone())
                                >"Shuffle all members"</button>
                            </div>
                            <div class="team-draft-member-list">
                                <For
                                    each=move || current_draft.get().map(|(_, draft)| {
                                        draft.members.into_iter().map(|member| member.id).collect::<Vec<_>>()
                                    }).unwrap_or_default()
                                    key=|member_id| member_id.clone()
                                    let:member_id
                                >
                                    <DraftMemberRow
                                        host_id=host_id.clone()
                                        draft_id=draft_id_for_rows.clone()
                                        member_id=member_id
                                    />
                                </For>
                            </div>
                            <button
                                class="settings-btn team-draft-add-report"
                                type="button"
                                on:click=move |_| send_add_report.run(draft_id_for_add.clone())
                            >"+ Add report"</button>
                            <Show when=move || error_sig.get().is_some() || command_error().is_some()>
                                <p class="settings-error">
                                    {move || error_sig.get().or_else(command_error).unwrap_or_default()}
                                </p>
                            </Show>
                            <div class="settings-form-footer">
                                <button
                                    class="settings-btn"
                                    type="button"
                                    disabled=move || submitting.get()
                                    on:click=move |_| send_discard.run(draft_id_for_discard.clone())
                                >"Discard"</button>
                                <button
                                    class="settings-btn settings-btn-primary team-draft-commit"
                                    type="button"
                                    disabled=move || submitting.get()
                                    on:click=move |_| send_commit.run(draft_id_for_commit.clone())
                                >{move || if submitting.get() { "Creating…" } else { "Create team" }}</button>
                            </div>
                        </div>
                    }.into_any()
                }
            }}
        </ModalOverlay>
    }
}

#[component]
fn DraftMemberRow(
    host_id: String,
    draft_id: TeamDraftId,
    member_id: TeamDraftMemberId,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_tiers = state.clone();

    let host_for_member = host_id.clone();
    let draft_for_member = draft_id.clone();
    let member_for_lookup = member_id.clone();
    let state_for_member = state.clone();
    let member: Memo<Option<TeamDraftMember>> = Memo::new(move |_| {
        state_for_member.team_drafts.with(|drafts| {
            drafts
                .get(&host_for_member)?
                .get(&draft_for_member)?
                .members
                .iter()
                .find(|member| member.id == member_for_lookup)
                .cloned()
        })
    });

    let catalog_state = state.clone();
    let host_for_catalog = host_id.clone();
    let catalog = Memo::new(move |_| {
        catalog_state
            .team_preset_catalogs
            .with(|catalogs| catalogs.get(&host_for_catalog).cloned())
    });

    let state_for_agents = state.clone();
    let host_for_agents = host_id.clone();
    let available_agents: Memo<Vec<CustomAgent>> = Memo::new(move |_| {
        let mut agents: Vec<CustomAgent> = state_for_agents
            .custom_agents
            .get()
            .get(&host_for_agents)
            .cloned()
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        agents.retain(|a| a.id.0 != crate::state::DEFAULT_CUSTOM_AGENT_ID);
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    });

    let state_for_backends = state.clone();
    let host_for_backends = host_id.clone();
    let available_backends: Memo<Vec<BackendKind>> = Memo::new(move |_| {
        state_for_backends
            .host_settings_by_host
            .get()
            .get(&host_for_backends)
            .map(|settings| settings.enabled_backends.clone())
            .unwrap_or_default()
    });

    let send_replace: Callback<TeamDraftMember> = {
        let state = state.clone();
        let host_id = host_id.clone();
        let draft_id = draft_id.clone();
        Callback::new(move |updated: TeamDraftMember| {
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                log::error!("team draft replace: host stream missing for {host_id}");
                return;
            };
            let host = host_id.clone();
            let draft = draft_id.clone();
            // The protocol only accepts the user-editable fields here;
            // org_role/profile stay server-owned and move through
            // dedicated update events (SetMemberProfile/shuffle/template).
            let edit = TeamDraftMemberEdit {
                id: updated.id,
                name: updated.name,
                description: updated.description,
                custom_agent_id: updated.custom_agent_id,
                backend_kind: updated.backend_kind,
                cost_hint: updated.cost_hint,
                project_ids: updated.project_ids,
            };
            spawn_local(async move {
                if let Err(error) = team_draft_replace_member(&host, stream, draft, edit).await {
                    log::error!("team_draft_replace_member failed: {error}");
                }
            });
        })
    };

    let send_profile: Callback<(
        Option<TeamRolePresetId>,
        Option<TeamPersonalityPresetId>,
        Vec<TeamPersonalityTrait>,
    )> = {
        let state = state.clone();
        let host_id = host_id.clone();
        let draft_id = draft_id.clone();
        let member_id = member_id.clone();
        Callback::new(
            move |(role_preset_id, personality_preset_id, personality_traits)| {
                let Some(stream) = state.host_stream_untracked(&host_id) else {
                    log::error!("team draft profile: host stream missing for {host_id}");
                    return;
                };
                let host = host_id.clone();
                let payload = protocol::TeamDraftUpdatePayload::SetMemberProfile {
                    draft_id: draft_id.clone(),
                    member_id: member_id.clone(),
                    role_preset_id,
                    personality_preset_id,
                    personality_traits,
                };
                spawn_local(async move {
                    if let Err(error) = team_draft_set_member_profile(&host, stream, payload).await
                    {
                        log::error!("team_draft_set_member_profile failed: {error}");
                    }
                });
            },
        )
    };

    let send_shuffle_member: Callback<TeamDraftShuffleScope> = {
        let state = state.clone();
        let host_id = host_id.clone();
        let draft_id = draft_id.clone();
        let member_id = member_id.clone();
        Callback::new(move |scope: TeamDraftShuffleScope| {
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                log::error!("team draft shuffle: host stream missing for {host_id}");
                return;
            };
            let host = host_id.clone();
            let draft = draft_id.clone();
            let member = member_id.clone();
            spawn_local(async move {
                if let Err(error) =
                    team_draft_shuffle(&host, stream, draft, Some(member), scope).await
                {
                    log::error!("team_draft_shuffle failed: {error}");
                }
            });
        })
    };

    let send_remove: Callback<()> = {
        let state = state.clone();
        let host_id = host_id.clone();
        let draft_id = draft_id.clone();
        let member_id = member_id.clone();
        Callback::new(move |_: ()| {
            let Some(stream) = state.host_stream_untracked(&host_id) else {
                log::error!("team draft remove: host stream missing for {host_id}");
                return;
            };
            let host = host_id.clone();
            let draft = draft_id.clone();
            let member = member_id.clone();
            spawn_local(async move {
                if let Err(error) = team_draft_remove_member(&host, stream, draft, member).await {
                    log::error!("team_draft_remove_member failed: {error}");
                }
            });
        })
    };

    // Surface the member's current `project_ids` as a comma-separated
    // attribute so the upfront Project picker's effect on each member is
    // observable from the DOM (E2E + wasm test verification, and a
    // future visual badge if we want one). Read-only — the upfront
    // picker is still the only edit surface in this wizard.
    let project_ids_attr = move || {
        member
            .get()
            .map(|m| {
                m.project_ids
                    .iter()
                    .map(|p| p.0.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    view! {
        <div
            class="team-draft-member-card"
            data-draft-member-id=member_id.0.clone()
            data-project-ids=project_ids_attr
        >
            <div class="team-draft-member-header">
                <strong>{move || member.get().map(|member| member.name).filter(|name| !name.is_empty()).unwrap_or_else(|| "Unnamed member".to_owned())}</strong>
                <span class="team-member-role-badge">
                    {move || match member.get().map(|member| member.org_role) {
                        Some(TeamMemberRole::Manager) => "Manager",
                        Some(TeamMemberRole::Report) => "Report",
                        None => "",
                    }}
                </span>
                <button
                    class="settings-btn team-draft-shuffle-member"
                    type="button"
                    on:click=move |_| send_shuffle_member.run(TeamDraftShuffleScope::Member)
                >"Shuffle member"</button>
                <button
                    class="settings-btn team-draft-shuffle-personality"
                    type="button"
                    on:click=move |_| send_shuffle_member.run(TeamDraftShuffleScope::Personality)
                >"Shuffle personality"</button>
                {move || matches!(member.get().map(|member| member.org_role), Some(TeamMemberRole::Report)).then(|| view! {
                    <button class="settings-btn" type="button" on:click=move |_| send_remove.run(())>"Remove"</button>
                })}
            </div>
            <div class="team-draft-grid">
                <label class="settings-form-label">
                    <span>"Role / specialty"</span>
                    <select
                        class="settings-text-input team-draft-role-select"
                        on:change=move |ev| {
                            let value = event_target_value(&ev);
                            let role = (!value.is_empty()).then_some(TeamRolePresetId(value));
                            let personality = member.get_untracked().and_then(|member| member.profile.and_then(|profile| profile.personality_preset_id));
                            send_profile.run((role, personality, Vec::new()));
                        }
                    >
                        <option value="" prop:selected=move || member.get().and_then(|m| m.profile.and_then(|p| p.role_preset_id)).is_none()>"Manual / no preset"</option>
                        {move || catalog.get().map(|catalog| {
                            catalog.role_presets.into_iter().map(|preset| {
                                let id = preset.id.clone();
                                let value = preset.id.0.clone();
                                view! {
                                    <option
                                        value=value
                                        prop:selected=move || member.get().and_then(|m| m.profile.and_then(|p| p.role_preset_id)) == Some(id.clone())
                                    >{preset.name}</option>
                                }
                            }).collect_view()
                        })}
                    </select>
                </label>
                <label class="settings-form-label">
                    <span>"Personality"</span>
                    <select
                        class="settings-text-input team-draft-personality-select"
                        on:change=move |ev| {
                            let value = event_target_value(&ev);
                            let personality = (!value.is_empty()).then_some(TeamPersonalityPresetId(value));
                            let role = member.get_untracked().and_then(|member| member.profile.and_then(|profile| profile.role_preset_id));
                            send_profile.run((role, personality, Vec::new()));
                        }
                    >
                        <option value="" prop:selected=move || member.get().and_then(|m| m.profile.and_then(|p| p.personality_preset_id)).is_none()>"Manual / no preset"</option>
                        {move || catalog.get().map(|catalog| {
                            catalog.personality_presets.into_iter().map(|preset| {
                                let id = preset.id.clone();
                                let value = preset.id.0.clone();
                                view! {
                                    <option
                                        value=value
                                        prop:selected=move || member.get().and_then(|m| m.profile.and_then(|p| p.personality_preset_id)) == Some(id.clone())
                                    >{preset.name}</option>
                                }
                            }).collect_view()
                        })}
                    </select>
                </label>
                <label class="settings-form-label">
                    <span>"Name"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || member.get().map(|member| member.name).unwrap_or_default()
                        on:input=move |ev| {
                            let value = event_target_value(&ev);
                            if let Some(mut current) = member.get_untracked() {
                                current.name = value;
                                send_replace.run(current);
                            }
                        }
                    />
                </label>
                <label class="settings-form-label">
                    <span>"Description"</span>
                    <input
                        class="settings-text-input"
                        type="text"
                        prop:value=move || member.get().map(|member| member.description).unwrap_or_default()
                        on:input=move |ev| {
                            let value = event_target_value(&ev);
                            if let Some(mut current) = member.get_untracked() {
                                current.description = value;
                                send_replace.run(current);
                            }
                        }
                    />
                </label>
                <label class="settings-form-label">
                    <span>"Custom agent"</span>
                    <select
                        class="settings-text-input"
                        on:change=move |ev| {
                            let value = event_target_value(&ev);
                            if let Some(mut current) = member.get_untracked() {
                                current.custom_agent_id = (!value.is_empty()).then_some(CustomAgentId(value));
                                send_replace.run(current);
                            }
                        }
                    >
                        <option value="" prop:selected=move || member.get().and_then(|member| member.custom_agent_id).is_none()>"Default agent"</option>
                        {move || available_agents.get().into_iter().map(|agent| {
                            let id = agent.id.clone();
                            let value = agent.id.0.clone();
                            view! {
                                <option
                                    value=value
                                    prop:selected=move || member.get().and_then(|member| member.custom_agent_id) == Some(id.clone())
                                >{agent.name}</option>
                            }
                        }).collect_view()}
                    </select>
                </label>
                <label class="settings-form-label">
                    <span>"Backend"</span>
                    <select
                        class="settings-text-input team-draft-backend-select"
                        on:change=move |ev| {
                            let parsed = parse_backend_kind(&event_target_value(&ev));
                            if let Some(mut current) = member.get_untracked() {
                                current.backend_kind = parsed;
                                send_replace.run(current);
                            }
                        }
                    >
                        <option value="" prop:selected=move || member.get().and_then(|member| member.backend_kind).is_none()>"— select backend —"</option>
                        {move || available_backends.get().into_iter().map(|backend| {
                            let value = backend_kind_value(backend);
                            let label = backend_kind_label(backend);
                            view! {
                                <option
                                    value=value
                                    prop:selected=move || member.get().and_then(|member| member.backend_kind) == Some(backend)
                                >{label}</option>
                            }
                        }).collect_view()}
                    </select>
                </label>
                {move || {
                    let tiers_enabled = state_for_tiers
                        .selected_host_settings()
                        .is_some_and(|settings| settings.complexity_tiers_enabled);
                    tiers_enabled.then(|| view! {
                        <label class="settings-form-label">
                            <span>"Task complexity"</span>
                            <select
                                class="settings-text-input"
                                on:change=move |ev| {
                                    let parsed = parse_cost_hint(&event_target_value(&ev));
                                    if let Some(mut current) = member.get_untracked() {
                                        current.cost_hint = parsed;
                                        send_replace.run(current);
                                    }
                                }
                            >
                                <option value="" prop:selected=move || member.get().and_then(|member| member.cost_hint).is_none()>"Backend default"</option>
                                {[SpawnCostHint::Low, SpawnCostHint::High]
                                    .into_iter()
                                    .map(|hint| {
                                        let value = cost_hint_value(hint);
                                        let label = cost_hint_label(hint);
                                        view! {
                                            <option
                                                value=value
                                                prop:selected=move || member.get().and_then(|member| member.cost_hint) == Some(hint)
                                            >{label}</option>
                                        }
                                    })
                                    .collect_view()}
                            </select>
                        </label>
                    })
                }}
            </div>
        </div>
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
    let form_for_apply = form.clone();
    let is_editing = editing_id.is_some();
    let title = if is_editing {
        "Edit member"
    } else if form.is_manager {
        "Replace manager"
    } else {
        "Add report"
    };

    // Track the last suggestion serial we've applied so the Effect only
    // applies *new* server-emitted suggestions. Reading the latest serial
    // at dialog open captures the baseline so a stale suggestion sitting
    // in state from a prior dialog is not auto-applied here.
    let baseline_serial: u64 = {
        let host_id = state.selected_host_id.get_untracked();
        let team_id_for_baseline = team_id.clone();
        host_id
            .and_then(|host_id| {
                state.team_member_shuffle_suggestions.with_untracked(|map| {
                    map.get(&host_id)
                        .and_then(|m| m.get(&team_id_for_baseline))
                        .map(|entry| entry.serial)
                })
            })
            .unwrap_or(0)
    };
    let last_applied_serial: RwSignal<u64> = RwSignal::new(baseline_serial);

    let state_for_shuffle = state.clone();
    let team_id_for_shuffle = team_id.clone();
    let on_shuffle = move |_| {
        request_member_shuffle(&state_for_shuffle, &team_id_for_shuffle, error_sig);
    };

    // Apply server-emitted suggestions onto the form's editable signals
    // when a new (higher-serial) suggestion arrives for this dialog's team.
    let state_for_apply = state.clone();
    let team_id_for_apply = team_id.clone();
    Effect::new(move |_| {
        let Some(host_id) = state_for_apply.selected_host_id.get() else {
            return;
        };
        let entry = state_for_apply.team_member_shuffle_suggestions.with(|map| {
            map.get(&host_id)
                .and_then(|m| m.get(&team_id_for_apply))
                .cloned()
        });
        let Some(entry) = entry else {
            return;
        };
        if entry.serial <= last_applied_serial.get_untracked() {
            return;
        }
        last_applied_serial.set(entry.serial);
        let suggestion = entry.suggestion;
        form_for_apply.name.set(suggestion.name);
        form_for_apply.description.set(suggestion.description);
        form_for_apply
            .custom_agent_id
            .set(suggestion.custom_agent_id);
        form_for_apply.profile.set(Some(suggestion.profile));
    });

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
            <div class="settings-confirm-header">
                <h3 class="settings-confirm-title">{title}</h3>
                {(!is_editing).then(|| view! {
                    <button
                        class="settings-btn member-dialog-shuffle"
                        type="button"
                        on:click=on_shuffle
                    >"Shuffle"</button>
                })}
            </div>
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

/// Fire a typed `TeamMemberShuffle` event so the server picks the random
/// role/personality. The server emits a `TeamMemberShuffleSuggestionNotify`
/// which dispatch stores; the dialog's Effect then applies the suggestion
/// to the open form. The frontend never picks semantic names, agents, or
/// personalities locally.
fn request_member_shuffle(state: &AppState, team_id: &TeamId, error_sig: RwSignal<Option<String>>) {
    let Some(host_id) = state.selected_host_id.get_untracked() else {
        error_sig.set(Some("No host selected.".to_string()));
        return;
    };
    let Some(stream) = state.host_stream_untracked(&host_id) else {
        error_sig.set(Some("Host is not connected.".to_string()));
        return;
    };
    error_sig.set(None);
    let team_id = team_id.clone();
    spawn_local(async move {
        if let Err(error) = team_member_shuffle(&host_id, stream, team_id).await {
            log::error!("team_member_shuffle failed: {error}");
            error_sig.set(Some(error));
        }
    });
}

#[component]
fn MemberFormFields(form: MemberFormState) -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_tiers = state.clone();
    let name_sig = form.name;
    let description_sig = form.description;
    let custom_agent_sig = form.custom_agent_id;
    let backend_sig = form.backend_kind;
    let cost_sig = form.cost_hint;
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
        agents.retain(|a| a.id.0 != crate::state::DEFAULT_CUSTOM_AGENT_ID);
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

    let state_for_backends = state.clone();
    let available_backends: Memo<Vec<BackendKind>> = Memo::new(move |_| {
        let Some(host_id) = state_for_backends.selected_host_id.get() else {
            return Vec::new();
        };
        state_for_backends
            .host_settings_by_host
            .get()
            .get(&host_id)
            .map(|settings| settings.enabled_backends.clone())
            .unwrap_or_default()
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
                    "Default agent"
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
                <span class="settings-form-hint">"The agent profile is fixed once a member exists."</span>
            })}
        </label>
        <label class="settings-form-label">
            <span>"Backend"</span>
            <select
                class="settings-text-input"
                disabled=is_editing
                on:change=move |ev| {
                    backend_sig.set(parse_backend_kind(&event_target_value(&ev)));
                }
            >
                <option
                    value=""
                    prop:selected=move || backend_sig.get().is_none()
                >
                    "— select backend —"
                </option>
                {move || available_backends.get().into_iter().map(|backend| {
                    let value = backend_kind_value(backend);
                    let label = backend_kind_label(backend);
                    view! {
                        <option
                            value=value
                            prop:selected=move || backend_sig.get() == Some(backend)
                        >
                            {label}
                        </option>
                    }
                }).collect_view()}
            </select>
            {move || available_backends.get().is_empty().then(|| view! {
                <span class="settings-form-hint">"No enabled backends on this host."</span>
            })}
            {move || is_editing.then(|| view! {
                <span class="settings-form-hint">"The backend is fixed once a member exists."</span>
            })}
        </label>
        {move || {
            let tiers_enabled = state_for_tiers
                .selected_host_settings()
                .is_some_and(|settings| settings.complexity_tiers_enabled);
            tiers_enabled.then(|| view! {
                <label class="settings-form-label">
                    <span>"Task complexity"</span>
                    <select
                        class="settings-text-input"
                        disabled=is_editing
                        on:change=move |ev| {
                            cost_sig.set(parse_cost_hint(&event_target_value(&ev)));
                        }
                    >
                        <option
                            value=""
                            prop:selected=move || cost_sig.get().is_none()
                        >
                            "Backend default"
                        </option>
                        {[SpawnCostHint::Low, SpawnCostHint::High]
                            .into_iter()
                            .map(|hint| {
                                let value = cost_hint_value(hint);
                                let label = cost_hint_label(hint);
                                view! {
                                    <option
                                        value=value
                                        prop:selected=move || cost_sig.get() == Some(hint)
                                    >
                                        {label}
                                    </option>
                                }
                            })
                            .collect_view()}
                    </select>
                    {move || is_editing.then(|| view! {
                        <span class="settings-form-hint">"The task complexity is fixed once a member exists."</span>
                    })}
                </label>
            })
        }}
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

pub(crate) fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
    }
}

fn backend_kind_value(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "tycode",
        BackendKind::Kiro => "kiro",
        BackendKind::Claude => "claude",
        BackendKind::Codex => "codex",
        BackendKind::Antigravity => "antigravity",
    }
}

fn parse_backend_kind(value: &str) -> Option<BackendKind> {
    match value {
        "tycode" => Some(BackendKind::Tycode),
        "kiro" => Some(BackendKind::Kiro),
        "claude" => Some(BackendKind::Claude),
        "codex" => Some(BackendKind::Codex),
        "antigravity" => Some(BackendKind::Antigravity),
        _ => None,
    }
}

fn agent_control_status_label(status: AgentControlStatus) -> &'static str {
    match status {
        AgentControlStatus::Thinking => "thinking",
        AgentControlStatus::Idle => "idle",
        AgentControlStatus::Failed => "failed",
    }
}

fn agent_control_status_dot_class(status: AgentControlStatus) -> &'static str {
    match status {
        AgentControlStatus::Thinking => "running",
        AgentControlStatus::Idle => "completed",
        AgentControlStatus::Failed => "error",
    }
}

fn cost_hint_label(cost_hint: SpawnCostHint) -> &'static str {
    match cost_hint {
        SpawnCostHint::Low => "Low",
        SpawnCostHint::Medium => "Medium",
        SpawnCostHint::High => "High",
    }
}

fn cost_hint_value(cost_hint: SpawnCostHint) -> &'static str {
    match cost_hint {
        SpawnCostHint::Low => "low",
        SpawnCostHint::Medium => "medium",
        SpawnCostHint::High => "high",
    }
}

fn parse_cost_hint(value: &str) -> Option<SpawnCostHint> {
    match value {
        "low" => Some(SpawnCostHint::Low),
        "medium" => Some(SpawnCostHint::Medium),
        "high" => Some(SpawnCostHint::High),
        _ => None,
    }
}

pub(crate) fn cost_hint_suffix(cost_hint: Option<SpawnCostHint>) -> String {
    cost_hint
        .map(|hint| format!(" · {}", cost_hint_label(hint)))
        .unwrap_or_default()
}

#[component]
fn ModalOverlay(
    on_close: Callback<()>,
    #[prop(optional)] wide: bool,
    children: Children,
) -> impl IntoView {
    // Deliberately do NOT dismiss on backdrop click: these wizards carry
    // multi-step form state (name, manager spec, finalized reports). A stray
    // click outside the modal used to silently throw all of that away with no
    // feedback, which surfaced as "I clicked Finish and nothing happened" —
    // in reality the wizard had been dismissed before Finish was clicked.
    // The user closes via the explicit Cancel button or Escape.
    let close_on_keydown = on_close;
    let modal_class = if wide {
        "settings-confirm-modal settings-confirm-modal-wide"
    } else {
        "settings-confirm-modal"
    };
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
            <div class=modal_class>
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
    let target_project =
        member
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
        if !crate::bridge::confirm_dialog("Delete team", "Delete this team? This cannot be undone.")
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
        Team, TeamDraft, TeamDraftId, TeamDraftMember, TeamDraftMemberId, TeamId, TeamMember,
        TeamMemberBindingPayload, TeamMemberId, TeamMemberPresetProfile, TeamMemberRole,
        TeamMemberShuffleSuggestion, TeamMemberShuffleSuggestionNotifyPayload, TeamMemberState,
        TeamPersonalityPresetId, TeamPersonalityTrait, TeamRolePresetId, TeamTemplateId,
        ToolPolicy,
    };
    use std::collections::HashMap;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Inject the production stylesheet once per test session so the layout
    /// assertions in tests that probe geometry (e.g. modal sizing) reflect
    /// real styling. Tests that only check DOM structure can skip this.
    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-teams")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-teams");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

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
            profile: None,
            custom_agent_id: Some(protocol::CustomAgentId("ca-1".to_owned())),
            backend_kind: BackendKind::Claude,
            cost_hint: None,
            session_id: None,
            project_ids: vec![protocol::ProjectId("p-1".to_owned())],
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn install_state(host_id: &str, teams: Vec<Team>, members: Vec<TeamMember>) -> AppState {
        let state = AppState::new();
        state.selected_host_id.set(Some(host_id.to_owned()));
        state.host_settings_by_host.update(|m| {
            m.insert(
                host_id.to_owned(),
                protocol::HostSettings {
                    enabled_backends: vec![BackendKind::Claude, BackendKind::Codex],
                    default_backend: Some(BackendKind::Claude),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: true,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                },
            );
        });
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
    async fn member_row_exposes_binding_status_via_dot_not_text() {
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

        // No status indicator until a binding arrives.
        assert!(
            container
                .query_selector(".team-member-status-dot")
                .unwrap()
                .is_none(),
            "no status indicator before binding arrives"
        );
        let row_before: HtmlElement = container
            .query_selector(".team-member-row")
            .unwrap()
            .expect("member row should render even before a binding")
            .dyn_into()
            .unwrap();
        let pre_text = row_before.text_content().unwrap_or_default();
        assert!(
            !pre_text.to_lowercase().contains("thinking")
                && !pre_text.to_lowercase().contains("idle")
                && !pre_text.to_lowercase().contains("failed"),
            "row text should not contain literal status words before binding: {pre_text:?}"
        );

        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: None,
                    status: AgentControlStatus::Thinking,
                    last_active_at_ms: Some(123),
                },
            );
        });
        next_tick().await;

        // After the binding arrives, the row exposes status as a dot
        // indicator with role=img and an accessible label / tooltip
        // naming the state. The literal status word must NOT appear in
        // the row's visible text — the compact-row redesign deliberately
        // moved that surface to title / aria-label.
        let dot: HtmlElement = container
            .query_selector(".team-member-status-dot")
            .unwrap()
            .expect("status indicator should appear after binding arrives")
            .dyn_into()
            .unwrap();
        assert_eq!(
            dot.get_attribute("role").as_deref(),
            Some("img"),
            "status indicator should advertise role=img so AT treats it as a labelled graphic"
        );
        assert_eq!(
            dot.get_attribute("aria-label").as_deref(),
            Some("thinking"),
            "status indicator should label itself with the binding status"
        );
        assert_eq!(
            dot.get_attribute("title").as_deref(),
            Some("thinking"),
            "status indicator should tooltip with the binding status"
        );

        let row_after: HtmlElement = container
            .query_selector(".team-member-row")
            .unwrap()
            .expect("member row should still render after binding")
            .dyn_into()
            .unwrap();
        let row_text = row_after.text_content().unwrap_or_default();
        let row_text_lc = row_text.to_lowercase();
        for word in ["thinking", "idle", "failed"] {
            assert!(
                !row_text_lc.contains(word),
                "row visible text should not contain literal status word {word:?} \
                 after binding arrives (status surface is the dot's aria-label / title); \
                 row text was: {row_text:?}"
            );
        }
        // The last-active marker still belongs in visible text — that's
        // human-readable context, not a status word.
        assert!(
            row_text.contains("last active recorded"),
            "expected last-active text after binding update: {row_text:?}"
        );
    }

    /// Compact icon on a `MemberRow` only shows up when the team member
    /// has a live binding (`current_agent_id` is `Some`) AND that binding
    /// is `Idle` AND the bound agent is in `state.agents` (so we can
    /// route to its instance stream) AND the host is connected.
    /// Clicking it (through the OK-stubbed confirm dialog) sends a real
    /// `AgentCompact` frame targeting the *bound agent's* instance
    /// stream — not the host stream — and flips the in-progress flag.
    #[wasm_bindgen_test]
    async fn member_row_compact_gated_on_bound_idle_and_routes_to_agent() {
        let calls = install_send_stub();
        let _ = js_sys::eval(
            r#"
            window.__TAURI__.core.invoke = function(cmd, args) {
                window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                if (cmd === 'plugin:dialog|message') {
                    return Promise.resolve('Ok');
                }
                return Promise.resolve();
            };
            "#,
        );

        let host_id = "host-compact";
        let manager_id = TeamMemberId("m-mgr".to_owned());
        let bound_agent_id = AgentId("a-mgr".to_owned());
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-mgr")],
            vec![make_member(
                "m-mgr",
                "t-1",
                "Manager",
                TeamMemberRole::Manager,
            )],
        );
        install_host_stream(&state, host_id);
        state.connection_statuses.update(|m| {
            m.insert(host_id.to_owned(), ConnectionStatus::Connected);
        });
        // The compact handler reaches for the bound agent's
        // instance_stream from state.agents — install one.
        state.agents.update(|agents| {
            agents.push(crate::state::AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: bound_agent_id.clone(),
                name: "Manager Agent".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                // Mirror the real backend format `/agent/<id>/<uuid>`.
                // Using a stable suffix keeps tests deterministic.
                instance_stream: StreamPath("/agent/a-mgr/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            });
        });

        let container = make_container();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        // No binding yet → no Compact icon. The other action icons
        // (Edit, Delete) should still be present per the existing UX so
        // this assertion is *narrow*: only the compact icon is gated.
        assert!(
            container
                .query_selector(".team-member-icon-btn-compact")
                .unwrap()
                .is_none(),
            "compact icon must be hidden when the member has no live binding"
        );

        // Bind, but mark as Thinking. Still hidden.
        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: Some(bound_agent_id.clone()),
                    status: AgentControlStatus::Thinking,
                    last_active_at_ms: Some(123),
                },
            );
        });
        next_tick().await;
        assert!(
            container
                .query_selector(".team-member-icon-btn-compact")
                .unwrap()
                .is_none(),
            "compact icon must be hidden while the binding is Thinking"
        );

        // Flip to Idle. Compact icon should now render.
        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: Some(bound_agent_id.clone()),
                    status: AgentControlStatus::Idle,
                    last_active_at_ms: Some(456),
                },
            );
        });
        next_tick().await;

        let btn: HtmlElement = container
            .query_selector(".team-member-icon-btn-compact")
            .unwrap()
            .expect("compact icon should render for Idle bound member")
            .dyn_into()
            .unwrap();
        assert_eq!(
            btn.get_attribute("aria-label").as_deref(),
            Some("Compact agent")
        );

        // Click → confirm dialog (Ok) → spawn_local → real
        // `AgentCompact` frame on the agent's instance stream.
        btn.click();
        for _ in 0..8 {
            next_tick().await;
        }

        // The compact button must surface accurate wording in the
        // confirmation dialog: backend marks the predecessor session
        // non-resumable, so the dialog must not promise the user they
        // can pick it back up. Walk the recorded invoke calls,
        // find the `plugin:dialog|message` invocation, and assert on
        // the `message` arg.
        let mut dialog_message: Option<String> = None;
        for entry in calls.iter() {
            let arr = entry.dyn_into::<js_sys::Array>().expect("array");
            if arr.get(0).as_string().as_deref() != Some("plugin:dialog|message") {
                continue;
            }
            let args_json = arr.get(1).as_string().expect("args");
            let args: JsonValue = serde_json::from_str(&args_json).expect("args parse");
            if let Some(msg) = args.get("message").and_then(|v| v.as_str()) {
                dialog_message = Some(msg.to_owned());
                break;
            }
        }
        let dialog_message = dialog_message
            .expect("team-member compact must open a confirm dialog before sending the frame");
        assert!(
            !dialog_message.to_lowercase().contains("can be resumed"),
            "team-member compact dialog must not promise the original session can be resumed; got: {dialog_message:?}"
        );
        assert!(
            dialog_message.contains("can't be resumed"),
            "team-member compact dialog must state the original session can't be resumed; got: {dialog_message:?}"
        );
        assert!(
            dialog_message.to_lowercase().contains("read-only")
                || dialog_message.to_lowercase().contains("read only"),
            "team-member compact dialog must mention the session remains as a read-only record; got: {dialog_message:?}"
        );

        let frames = recorded_frames(&calls);
        let compact_frames: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::AgentCompact.to_string())
            .collect();
        assert_eq!(
            compact_frames.len(),
            1,
            "team-member compact should fire exactly one AgentCompact frame, all frames: {frames:?}"
        );
        let envelope_for_stream = compact_frames[0].1.clone();
        // The frames vector loses the stream path; re-decode it from
        // the raw recorded send_host_line calls to assert it routes via
        // the bound agent's instance stream (not the host stream).
        let mut routed_to_agent_stream = false;
        for entry in calls.iter() {
            let arr = entry.dyn_into::<js_sys::Array>().expect("array");
            if arr.get(0).as_string().as_deref() != Some("send_host_line") {
                continue;
            }
            let args_json = arr.get(1).as_string().expect("args");
            let args: JsonValue = serde_json::from_str(&args_json).expect("args parse");
            let line = args.get("line").and_then(|v| v.as_str()).expect("line");
            let env: JsonValue = serde_json::from_str(line).expect("envelope parse");
            if env.get("kind").and_then(|v| v.as_str()) == Some("agent_compact") {
                routed_to_agent_stream =
                    env.get("stream").and_then(|v| v.as_str()) == Some("/agent/a-mgr/inst");
                break;
            }
        }
        assert!(
            routed_to_agent_stream,
            "AgentCompact must target the bound agent's instance stream /agent/a-mgr/inst"
        );
        assert_eq!(
            envelope_for_stream,
            serde_json::json!({}),
            "default AgentCompactPayload omits optional fields"
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&bound_agent_id)),
            "bound agent should be marked in-flight while the server processes"
        );

        // While in-flight, the compact icon is hidden again so the user
        // can't double-fire.
        next_tick().await;
        assert!(
            container
                .query_selector(".team-member-icon-btn-compact")
                .unwrap()
                .is_none(),
            "compact icon must be hidden while a compaction is already in flight for the bound agent"
        );
    }

    #[wasm_bindgen_test]
    async fn teams_panel_marks_active_live_team_member() {
        let container = make_container();
        let host_id = "host-active-live";
        let report_agent_id = AgentId("agent-report".to_owned());
        let report_id = TeamMemberId("m-2".to_owned());
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![
                make_member("m-1", "t-1", "Manager A", TeamMemberRole::Manager),
                make_member("m-2", "t-1", "Report One", TeamMemberRole::Report),
            ],
        );
        state.team_member_bindings.update(|m| {
            m.entry(host_id.to_owned()).or_default().insert(
                report_id.clone(),
                TeamMemberBindingPayload {
                    member_id: report_id.clone(),
                    current_agent_id: Some(report_agent_id.clone()),
                    status: AgentControlStatus::Idle,
                    last_active_at_ms: Some(987),
                },
            );
        });
        state.open_tab(
            TabContent::chat_with_agent(ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: report_agent_id,
            }),
            "Report One".to_owned(),
            true,
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let active_card = container
            .query_selector(".team-card-active")
            .unwrap()
            .expect("active team card should be marked");
        let active_card: HtmlElement = active_card.dyn_into().unwrap();
        assert_eq!(
            active_card.get_attribute("data-team-id").as_deref(),
            Some("t-1")
        );

        let active_row = container
            .query_selector(".team-member-row-active")
            .unwrap()
            .expect("active member row should be marked");
        let active_row: HtmlElement = active_row.dyn_into().unwrap();
        let text = active_row.text_content().unwrap_or_default();
        assert!(
            text.contains("Report One"),
            "active row should be the report chat: {text:?}"
        );
        assert!(
            text.contains("Active"),
            "active row should show an active marker: {text:?}"
        );
        assert!(
            text.contains("last active recorded"),
            "active row should still show binding last-active detail: {text:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn teams_panel_marks_active_draft_team_member() {
        let container = make_container();
        let host_id = "host-active-draft";
        let report_id = TeamMemberId("m-2".to_owned());
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![
                make_member("m-1", "t-1", "Manager A", TeamMemberRole::Manager),
                make_member("m-2", "t-1", "Report One", TeamMemberRole::Report),
            ],
        );
        state.open_tab(
            TabContent::team_member_draft(host_id.to_owned(), report_id),
            "Report One".to_owned(),
            true,
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let active_card = container
            .query_selector(".team-card-active")
            .unwrap()
            .expect("draft team card should be marked active");
        let active_card: HtmlElement = active_card.dyn_into().unwrap();
        assert_eq!(
            active_card.get_attribute("data-team-id").as_deref(),
            Some("t-1")
        );

        let active_row = container
            .query_selector(".team-member-row-active")
            .unwrap()
            .expect("draft team member row should be marked active");
        let active_row: HtmlElement = active_row.dyn_into().unwrap();
        let text = active_row.text_content().unwrap_or_default();
        assert!(
            text.contains("Report One"),
            "active draft row should be the pending report: {text:?}"
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

    /// Clicking a report row in the Teams panel opens that report's chat
    /// through the same 3-state `open_member_chat` flow used for the
    /// team-open click. We exercise the function directly and assert that a
    /// draft team-member tab is opened for the report.
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

    // ── New-team draft helpers ───────────────────────────────────────────────

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
        use protocol::{Project, ProjectId, ProjectSource};
        state.projects.update(|projects| {
            projects.push(ProjectInfo {
                host_id: host_id.to_owned(),
                project: Project {
                    id: ProjectId(project_id.to_owned()),
                    name: name.to_owned(),
                    source: ProjectSource::Standalone { roots: Vec::new() },
                    sort_order: 0,
                },
            });
        });
    }

    fn install_catalog(state: &AppState, host_id: &str) {
        use protocol::{
            TeamPersonalityPreset, TeamPersonalityTraitPreset, TeamPresetCatalog, TeamRolePreset,
            TeamTemplate, TeamTemplateMember,
        };
        state.team_preset_catalogs.update(|catalogs| {
            catalogs.insert(
                host_id.to_owned(),
                TeamPresetCatalog {
                    role_presets: vec![
                        TeamRolePreset {
                            id: TeamRolePresetId("tech-lead-planner".to_owned()),
                            name: "Tech lead / planner".to_owned(),
                            summary: "Plans work".to_owned(),
                            default_member_name: "Tech Lead".to_owned(),
                            default_description: "Plans the team".to_owned(),
                            default_custom_agent_id: Some(CustomAgentId(
                                "tyde-team-lead".to_owned(),
                            )),
                        },
                        TeamRolePreset {
                            id: TeamRolePresetId("frontend-specialist".to_owned()),
                            name: "Frontend specialist".to_owned(),
                            summary: "Owns UI".to_owned(),
                            default_member_name: "Frontend Specialist".to_owned(),
                            default_description: "Builds UI".to_owned(),
                            default_custom_agent_id: Some(CustomAgentId(
                                "tyde-frontend-engineer".to_owned(),
                            )),
                        },
                    ],
                    personality_traits: vec![TeamPersonalityTraitPreset {
                        trait_id: TeamPersonalityTrait::Pragmatic,
                        name: "Pragmatic".to_owned(),
                        summary: "Ships safely".to_owned(),
                    }],
                    personality_presets: vec![TeamPersonalityPreset {
                        id: TeamPersonalityPresetId("pragmatic-shipper".to_owned()),
                        name: "Pragmatic shipper".to_owned(),
                        summary: "Small shippable slice".to_owned(),
                        traits: vec![TeamPersonalityTrait::Pragmatic],
                    }],
                    team_templates: vec![
                        TeamTemplate {
                            id: TeamTemplateId("small-feature-team".to_owned()),
                            name: "Small feature team".to_owned(),
                            summary: "Balanced frontend/backend/test team".to_owned(),
                            balanced: true,
                            members: vec![TeamTemplateMember {
                                org_role: TeamMemberRole::Manager,
                                role_preset_id: TeamRolePresetId("tech-lead-planner".to_owned()),
                                personality_preset_id: Some(TeamPersonalityPresetId(
                                    "pragmatic-shipper".to_owned(),
                                )),
                                name: "Feature Lead".to_owned(),
                                description: "Coordinates the feature".to_owned(),
                            }],
                        },
                        TeamTemplate {
                            id: TeamTemplateId("solo-reviewer".to_owned()),
                            name: "Solo + reviewer".to_owned(),
                            summary: "Manager plus a focused review partner".to_owned(),
                            balanced: false,
                            members: vec![
                                TeamTemplateMember {
                                    org_role: TeamMemberRole::Manager,
                                    role_preset_id: TeamRolePresetId(
                                        "tech-lead-planner".to_owned(),
                                    ),
                                    personality_preset_id: Some(TeamPersonalityPresetId(
                                        "pragmatic-shipper".to_owned(),
                                    )),
                                    name: "Feature Lead".to_owned(),
                                    description: "Coordinates the feature".to_owned(),
                                },
                                TeamTemplateMember {
                                    org_role: TeamMemberRole::Report,
                                    role_preset_id: TeamRolePresetId(
                                        "frontend-specialist".to_owned(),
                                    ),
                                    personality_preset_id: Some(TeamPersonalityPresetId(
                                        "pragmatic-shipper".to_owned(),
                                    )),
                                    name: "Reviewer".to_owned(),
                                    description: "Reviews the implementation".to_owned(),
                                },
                            ],
                        },
                    ],
                },
            );
        });
    }

    fn make_draft_member(
        id: &str,
        org_role: TeamMemberRole,
        name: &str,
        description: &str,
    ) -> TeamDraftMember {
        TeamDraftMember {
            id: TeamDraftMemberId(id.to_owned()),
            org_role,
            name: name.to_owned(),
            description: description.to_owned(),
            profile: Some(TeamMemberPresetProfile {
                role_preset_id: Some(TeamRolePresetId("tech-lead-planner".to_owned())),
                personality_preset_id: Some(TeamPersonalityPresetId(
                    "pragmatic-shipper".to_owned(),
                )),
                personality_traits: vec![TeamPersonalityTrait::Pragmatic],
            }),
            custom_agent_id: None,
            backend_kind: None,
            cost_hint: None,
            project_ids: Vec::new(),
        }
    }

    fn install_draft(state: &AppState, host_id: &str, draft: TeamDraft) {
        state.team_drafts.update(|drafts| {
            drafts
                .entry(host_id.to_owned())
                .or_default()
                .insert(draft.id.clone(), draft);
        });
    }

    fn make_draft(name: &str, members: Vec<TeamDraftMember>) -> TeamDraft {
        TeamDraft {
            id: TeamDraftId("active-team-draft".to_owned()),
            name: name.to_owned(),
            members,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    /// Select the `<select>` nested in the form label with the given visible
    /// field name. This keeps tests tied to user-facing labels instead of DOM
    /// order.
    fn set_select_by_label(container: &HtmlElement, label_text: &str, value: &str) {
        let labels = container
            .query_selector_all("label.settings-form-label")
            .unwrap();
        for i in 0..labels.length() {
            let label = labels.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            let Some(text) = label
                .query_selector("span")
                .unwrap()
                .and_then(|span| span.text_content())
            else {
                continue;
            };
            if text.trim() != label_text {
                continue;
            }
            let select: web_sys::HtmlSelectElement = label
                .query_selector("select")
                .unwrap()
                .unwrap_or_else(|| panic!("label {label_text:?} has no select"))
                .dyn_into()
                .unwrap();
            select.set_value(value);
            let event = web_sys::Event::new("change").unwrap();
            select.dispatch_event(&event).unwrap();
            return;
        }
        panic!("select label {label_text:?} not found");
    }

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

    fn find_draft_member_text_input(
        container: &HtmlElement,
        member_id: &str,
        label_text: &str,
    ) -> web_sys::HtmlInputElement {
        let selector = format!(".team-draft-member-card[data-draft-member-id='{member_id}']");
        let member_card: HtmlElement = container
            .query_selector(&selector)
            .unwrap()
            .unwrap_or_else(|| panic!("no draft member card for {member_id}"))
            .dyn_into()
            .unwrap();
        let labels = member_card
            .query_selector_all("label.settings-form-label")
            .unwrap();
        for i in 0..labels.length() {
            let label = labels.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            let Some(text) = label
                .query_selector("span")
                .unwrap()
                .and_then(|span| span.text_content())
            else {
                continue;
            };
            if text.trim() != label_text {
                continue;
            }
            return label
                .query_selector("input[type='text']")
                .unwrap()
                .unwrap_or_else(|| panic!("label {label_text:?} has no text input"))
                .dyn_into()
                .unwrap();
        }
        panic!("text input label {label_text:?} not found for member {member_id}");
    }

    #[wasm_bindgen_test]
    async fn new_team_dialog_renders_catalog_and_sends_template_create() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-draft-catalog";
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ New team");
        next_tick().await;
        let text = visible_text(&container);
        assert!(
            text.contains("Start blank"),
            "blank start missing: {text:?}"
        );
        assert!(
            text.contains("Generate balanced team"),
            "balanced generation missing: {text:?}"
        );
        assert!(
            text.contains("Solo + reviewer"),
            "template catalog missing: {text:?}"
        );
        let template_cards = container
            .query_selector_all(".team-draft-template-card")
            .unwrap();
        let card_text = (0..template_cards.length())
            .map(|index| {
                template_cards
                    .item(index)
                    .and_then(|node| node.text_content())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !card_text.contains("Small feature team"),
            "balanced template should only appear as the generate button: {card_text:?}"
        );
        assert!(
            !text.contains("New team — name"),
            "old local wizard should not be rendered: {text:?}"
        );

        click_button_with_text(&container, "Generate balanced team");
        next_tick().await;
        let frames = recorded_frames(&calls);
        let creates: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::TeamDraftCreate.to_string())
            .collect();
        assert_eq!(creates.len(), 1, "expected TeamDraftCreate: {frames:?}");
        assert_eq!(
            creates[0]
                .1
                .get("template_id")
                .and_then(|value| value.as_str()),
            Some("small-feature-team")
        );
    }

    #[wasm_bindgen_test]
    async fn team_draft_notify_drives_member_controls_and_shuffle_sends_typed_events() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-draft-editor";
        let draft_id = TeamDraftId("active-team-draft".to_owned());
        let member_id = TeamDraftMemberId("draft-manager".to_owned());
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);
        install_draft(
            &state,
            host_id,
            make_draft(
                "Generated Team",
                vec![make_draft_member(
                    &member_id.0,
                    TeamMemberRole::Manager,
                    "Feature Lead",
                    "Coordinates the feature",
                )],
            ),
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;
        click_button_with_text(&container, "+ New team");
        next_tick().await;

        let name_input: web_sys::HtmlInputElement = container
            .query_selector("input.team-draft-name")
            .unwrap()
            .expect("draft name input")
            .dyn_into()
            .unwrap();
        assert_eq!(name_input.value(), "Generated Team");
        let text = visible_text(&container);
        assert!(
            text.contains("Feature Lead"),
            "draft member missing: {text:?}"
        );
        assert!(
            text.contains("Tech lead / planner") && text.contains("Pragmatic shipper"),
            "catalog-backed controls missing: {text:?}"
        );

        click_button_with_text(&container, "Shuffle member");
        next_tick().await;
        set_select_by_label(&container, "Role / specialty", "frontend-specialist");
        next_tick().await;
        let frames = recorded_frames(&calls);
        assert!(
            frames.iter().any(
                |(kind, payload)| kind == &FrameKind::TeamDraftShuffle.to_string()
                    && payload.get("draft_id") == Some(&JsonValue::String(draft_id.0.clone()))
                    && payload.get("member_id") == Some(&JsonValue::String(member_id.0.clone()))
            ),
            "expected member shuffle frame: {frames:?}"
        );
        assert!(
            frames.iter().any(|(kind, payload)| {
                kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("set_member_profile")
                    && payload.get("role_preset_id").and_then(|v| v.as_str())
                        == Some("frontend-specialist")
            }),
            "expected SetMemberProfile frame: {frames:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn team_draft_member_name_input_keeps_focus_across_server_echoes() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-draft-member-name-focus";
        let draft_id = TeamDraftId("active-team-draft".to_owned());
        let member_id = TeamDraftMemberId("draft-manager".to_owned());
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);
        install_draft(
            &state,
            host_id,
            make_draft(
                "Generated Team",
                vec![make_draft_member(
                    &member_id.0,
                    TeamMemberRole::Manager,
                    "Feature Lead",
                    "Coordinates the feature",
                )],
            ),
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;
        click_button_with_text(&container, "+ New team");
        next_tick().await;

        let document = web_sys::window().unwrap().document().unwrap();
        let name_input = find_draft_member_text_input(&container, &member_id.0, "Name");
        assert_eq!(
            name_input.value(),
            "Feature Lead",
            "member Name input should render the server-provided draft member name"
        );
        let name_input_node: web_sys::Element = name_input.clone().dyn_into().unwrap();
        let name_input_element: HtmlElement = name_input.clone().dyn_into().unwrap();
        name_input_element.focus().unwrap();

        let active = document.active_element().expect("focused element");
        assert!(
            active.is_same_node(Some(&name_input_node)),
            "member Name input should receive focus before typing"
        );

        for typed in ["Feature Lead X", "Feature Lead XY"] {
            name_input.set_value(typed);
            let event = web_sys::Event::new("input").unwrap();
            name_input.dispatch_event(&event).unwrap();
            next_tick().await;

            state.team_drafts.update(|drafts| {
                drafts
                    .get_mut(host_id)
                    .expect("host drafts")
                    .get_mut(&draft_id)
                    .expect("active draft")
                    .members
                    .iter_mut()
                    .find(|member| member.id == member_id)
                    .expect("draft manager")
                    .name = typed.to_owned();
            });
            next_tick().await;

            let current_input = find_draft_member_text_input(&container, &member_id.0, "Name");
            let current_node: web_sys::Element = current_input.clone().dyn_into().unwrap();
            assert!(
                name_input_node.is_same_node(Some(&current_node)),
                "member Name input was remounted after typing {typed:?}"
            );
            assert_eq!(
                current_input.value(),
                typed,
                "member Name input should keep the typed text"
            );
            let active = document.active_element().expect("focused element");
            assert!(
                active.is_same_node(Some(&name_input_node)),
                "member Name input lost focus after typing {typed:?}"
            );
        }

        let frames = recorded_frames(&calls);
        assert!(
            frames.iter().any(
                |(kind, payload)| kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("replace_member")
                    && payload
                        .get("member")
                        .and_then(|member| member.get("name"))
                        .and_then(|v| v.as_str())
                        == Some("Feature Lead XY")
            ),
            "expected final multi-character draft member name update: {frames:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn team_draft_editable_fields_and_commit_use_atomic_draft_events() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-draft-commit";
        let project_id = "p-1";
        let state = install_state(host_id, vec![], vec![]);
        state.host_settings_by_host.update(|settings_by_host| {
            settings_by_host
                .get_mut(host_id)
                .expect("test host settings")
                .complexity_tiers_enabled = true;
        });
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);
        install_project(&state, host_id, project_id, "Test Project");
        install_custom_agents(
            &state,
            host_id,
            vec![make_custom_agent("ca-1", "Custom teammate")],
        );
        install_draft(
            &state,
            host_id,
            make_draft(
                "",
                vec![make_draft_member(
                    "draft-manager",
                    TeamMemberRole::Manager,
                    "",
                    "",
                )],
            ),
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;
        click_button_with_text(&container, "+ New team");
        next_tick().await;

        set_nth_text_input(&container, 0, "Atomic Team");
        next_tick().await;
        set_nth_text_input(&container, 1, "Default Manager");
        next_tick().await;
        set_nth_text_input(&container, 2, "Coordinates the team");
        next_tick().await;
        set_select_by_label(&container, "Backend", "codex");
        next_tick().await;
        set_select_by_label(&container, "Task complexity", "low");
        next_tick().await;
        // The wizard's upfront Project picker self-seeds from the installed
        // project list and propagates to each member via the autofill
        // effect; no per-member project checkbox to click.
        let _ = project_id;
        click_button_with_text(&container, "Create team");
        next_tick().await;

        let frames = recorded_frames(&calls);
        assert!(
            frames.iter().any(
                |(kind, payload)| kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("set_name")
                    && payload.get("name").and_then(|v| v.as_str()) == Some("Atomic Team")
            ),
            "expected draft name update: {frames:?}"
        );
        assert!(
            frames.iter().any(
                |(kind, payload)| kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("replace_member")
                    && payload
                        .get("member")
                        .and_then(|member| member.get("backend_kind"))
                        .and_then(|v| v.as_str())
                        == Some("codex")
            ),
            "expected replace_member backend update: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|(kind, _)| kind == &FrameKind::TeamDraftCommit.to_string()),
            "expected atomic TeamDraftCommit: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .all(|(kind, _)| kind != &FrameKind::TeamCreate.to_string()
                    && kind != &FrameKind::TeamMemberCreate.to_string()),
            "new team flow must not use create-then-member loop: {frames:?}"
        );
    }

    /// The wizard auto-fills every draft member's backend from the host's
    /// `HostSettings.default_backend` and the upfront Project picker's
    /// selection (seeded from the user's currently-open project). Users
    /// only have to touch those fields when they want to override.
    #[wasm_bindgen_test]
    async fn new_team_wizard_autofills_backend_and_project_from_defaults() {
        use crate::state::ActiveProjectRef;
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-autofill";
        let active_project_id = "p-active";
        let other_project_id = "p-other";
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);
        // Two projects on the host; the autofill should pick the active
        // one, not just the first by name.
        install_project(&state, host_id, other_project_id, "Aardvark");
        install_project(&state, host_id, active_project_id, "Zebra");
        state.active_project.set(Some(ActiveProjectRef {
            host_id: host_id.to_owned(),
            project_id: ProjectId(active_project_id.to_owned()),
        }));
        install_draft(
            &state,
            host_id,
            make_draft(
                "",
                vec![make_draft_member(
                    "draft-manager",
                    TeamMemberRole::Manager,
                    "",
                    "",
                )],
            ),
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;
        click_button_with_text(&container, "+ New team");
        next_tick().await;
        // Two ticks: the project-seed effect runs on draft visibility,
        // then the autofill effect picks up the seeded project.
        next_tick().await;

        let frames = recorded_frames(&calls);
        let replaces: Vec<_> = frames
            .iter()
            .filter(|(kind, payload)| {
                kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("replace_member")
            })
            .collect();
        assert!(
            !replaces.is_empty(),
            "expected at least one autofill replace_member frame: {frames:?}"
        );
        let last = replaces
            .last()
            .expect("replace_member frames non-empty above");
        let member = last
            .1
            .get("member")
            .expect("replace_member carries member payload");
        assert_eq!(
            member.get("backend_kind").and_then(|v| v.as_str()),
            Some("claude"),
            "autofill should use the host's default_backend: {member:?}"
        );
        let project_ids = member
            .get("project_ids")
            .and_then(|v| v.as_array())
            .expect("project_ids should be an array");
        assert_eq!(project_ids.len(), 1, "exactly one project: {project_ids:?}");
        assert_eq!(
            project_ids[0].as_str(),
            Some(active_project_id),
            "autofill should seed the active project, not the first by name"
        );
    }

    /// Helper: drive the upfront wizard Project picker. Targets the class
    /// directly because the picker label includes an inline hint span
    /// that confuses the generic `set_select_by_label` text matcher.
    fn pick_wizard_project(container: &HtmlElement, value: &str) {
        let select: web_sys::HtmlSelectElement = container
            .query_selector("select.team-draft-project-select")
            .unwrap()
            .expect("upfront project picker should be visible in the wizard")
            .dyn_into()
            .unwrap();
        select.set_value(value);
        let event = web_sys::Event::new("change").unwrap();
        select.dispatch_event(&event).unwrap();
    }

    /// Treat the upfront Project picker as the single source of truth for
    /// every draft member: changing the selection must propagate to *all*
    /// existing members, not just the ones whose `project_ids` are empty.
    /// Without this, hiding per-member project controls leaves users
    /// unable to correct the stale selection.
    #[wasm_bindgen_test]
    async fn upfront_project_change_overrides_existing_member_project_ids() {
        use crate::state::ActiveProjectRef;
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-upfront-change";
        let project_a = "p-a";
        let project_b = "p-b";
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);
        install_project(&state, host_id, project_a, "Project A");
        install_project(&state, host_id, project_b, "Project B");
        state.active_project.set(Some(ActiveProjectRef {
            host_id: host_id.to_owned(),
            project_id: ProjectId(project_a.to_owned()),
        }));
        install_draft(
            &state,
            host_id,
            make_draft(
                "",
                vec![make_draft_member(
                    "draft-manager",
                    TeamMemberRole::Manager,
                    "",
                    "",
                )],
            ),
        );
        // Stand in for the server: when the frontend sends a replace_member
        // the real host echoes a TeamDraftNotify with the new field values.
        // Without an echo the autofill effect's `member.project_ids !=
        // project_target` predicate never flips false, so simulate it here
        // by mirroring the latest replace_member into `team_drafts`.
        let echo_state = state.clone();
        let echo_host = host_id.to_owned();
        let echo_calls = calls.clone();
        let echo = move || {
            let frames = recorded_frames(&echo_calls);
            let latest = frames.into_iter().rev().find(|(kind, payload)| {
                kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("replace_member")
            });
            let Some((_, payload)) = latest else { return };
            let Some(member) = payload.get("member") else {
                return;
            };
            let member_id = member
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let project_ids: Vec<ProjectId> = member
                .get("project_ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| ProjectId(s.to_owned())))
                        .collect()
                })
                .unwrap_or_default();
            let backend = member
                .get("backend_kind")
                .and_then(|v| v.as_str())
                .and_then(parse_backend_kind);
            echo_state.team_drafts.update(|map| {
                if let Some(drafts) = map.get_mut(&echo_host)
                    && let Some(draft) = drafts.values_mut().next()
                    && let Some(m) = draft.members.iter_mut().find(|m| m.id.0 == member_id)
                {
                    m.project_ids = project_ids;
                    if backend.is_some() {
                        m.backend_kind = backend;
                    }
                }
            });
        };

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;
        click_button_with_text(&container, "+ New team");
        next_tick().await;
        next_tick().await; // seed → autofill cascade
        echo(); // mirror the autofill A onto state
        next_tick().await;

        let frames_after_seed = recorded_frames(&calls);
        let last_replace_a = frames_after_seed
            .iter()
            .rev()
            .find(|(kind, payload)| {
                kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("replace_member")
            })
            .expect("seed autofill should send a replace_member");
        let seeded_projects = last_replace_a
            .1
            .get("member")
            .and_then(|m| m.get("project_ids"))
            .and_then(|v| v.as_array())
            .expect("seed replace_member carries project_ids");
        assert_eq!(seeded_projects.len(), 1);
        assert_eq!(seeded_projects[0].as_str(), Some(project_a));

        // No per-member project section in the wizard — this is the
        // contract that makes the upfront picker the only way to set
        // project membership. If this shape were ever silently restored
        // the user could be left with inconsistent state on picker
        // changes.
        let per_member_projects = container
            .query_selector_all(".team-draft-member-card .team-member-projects")
            .unwrap();
        assert_eq!(
            per_member_projects.length(),
            0,
            "wizard should not render per-member project pickers"
        );

        // The card's data-project-ids attribute reflects the live member
        // state and is the DOM surface E2E and visual debug can read.
        let card_before: HtmlElement = container
            .query_selector(".team-draft-member-card[data-draft-member-id='draft-manager']")
            .unwrap()
            .expect("draft manager card present")
            .dyn_into()
            .unwrap();
        assert_eq!(
            card_before.get_attribute("data-project-ids").as_deref(),
            Some(project_a),
            "card data-project-ids should reflect the seeded project A"
        );

        // Now change the upfront picker to project B and confirm the
        // member's project_ids is replaced with [project_b], not just
        // additive or ignored.
        pick_wizard_project(&container, project_b);
        next_tick().await;
        next_tick().await;
        echo();
        next_tick().await;

        let frames_after_change = recorded_frames(&calls);
        let last_replace_b = frames_after_change
            .iter()
            .rev()
            .find(|(kind, payload)| {
                kind == &FrameKind::TeamDraftUpdate.to_string()
                    && payload.get("kind").and_then(|v| v.as_str()) == Some("replace_member")
            })
            .expect("picker change should send a replace_member");
        let new_projects = last_replace_b
            .1
            .get("member")
            .and_then(|m| m.get("project_ids"))
            .and_then(|v| v.as_array())
            .expect("post-change replace_member carries project_ids");
        assert_eq!(
            new_projects.len(),
            1,
            "exactly one project after picker change: {new_projects:?}"
        );
        assert_eq!(
            new_projects[0].as_str(),
            Some(project_b),
            "picker change must rewrite member project_ids to the new selection"
        );
        let card_after: HtmlElement = container
            .query_selector(".team-draft-member-card[data-draft-member-id='draft-manager']")
            .unwrap()
            .expect("draft manager card present after picker change")
            .dyn_into()
            .unwrap();
        assert_eq!(
            card_after.get_attribute("data-project-ids").as_deref(),
            Some(project_b),
            "card data-project-ids should re-render to project B after picker change"
        );
    }

    /// Multi-member drafts (template-instantiated or generated) must all
    /// receive the upfront-picked project on autofill, not just the first.
    #[wasm_bindgen_test]
    async fn upfront_project_propagates_to_all_template_members() {
        use crate::state::ActiveProjectRef;
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-multi-member";
        let project_id = "p-1";
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);
        install_project(&state, host_id, project_id, "Test Project");
        state.active_project.set(Some(ActiveProjectRef {
            host_id: host_id.to_owned(),
            project_id: ProjectId(project_id.to_owned()),
        }));
        install_draft(
            &state,
            host_id,
            make_draft(
                "",
                vec![
                    make_draft_member("draft-manager", TeamMemberRole::Manager, "", ""),
                    make_draft_member("draft-report-1", TeamMemberRole::Report, "", ""),
                    make_draft_member("draft-report-2", TeamMemberRole::Report, "", ""),
                ],
            ),
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;
        click_button_with_text(&container, "+ New team");
        next_tick().await;
        next_tick().await;

        let frames = recorded_frames(&calls);
        // Group replace_member frames by member id and assert each member
        // saw at least one autofill that targets [project_id].
        use std::collections::HashSet;
        let mut covered: HashSet<String> = HashSet::new();
        for (kind, payload) in &frames {
            if kind != &FrameKind::TeamDraftUpdate.to_string() {
                continue;
            }
            if payload.get("kind").and_then(|v| v.as_str()) != Some("replace_member") {
                continue;
            }
            let Some(member) = payload.get("member") else {
                continue;
            };
            let project_ids = member
                .get("project_ids")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            if project_ids == vec![project_id]
                && let Some(id) = member.get("id").and_then(|v| v.as_str())
            {
                covered.insert(id.to_owned());
            }
        }
        for expected in ["draft-manager", "draft-report-1", "draft-report-2"] {
            assert!(
                covered.contains(expected),
                "expected autofill replace_member for {expected} with [{project_id}], saw {frames:?}"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn add_report_dialog_shuffle_sends_typed_event_and_applies_server_suggestion() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-shuffle";
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager",
                TeamMemberRole::Manager,
            )],
        );
        install_host_stream(&state, host_id);
        install_project(&state, host_id, "p-1", "Test Project");
        install_catalog(&state, host_id);
        // The custom agent select only carries options for installed agents;
        // install the agent the server-emitted suggestion will reference so
        // the dropdown can actually display it.
        install_custom_agents(
            &state,
            host_id,
            vec![make_custom_agent(
                "tyde-frontend-engineer",
                "Frontend Engineer",
            )],
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ Report");
        next_tick().await;

        let shuffle_btn: HtmlElement = container
            .query_selector(".member-dialog-shuffle")
            .unwrap()
            .expect("Add report dialog should expose a Shuffle button")
            .dyn_into()
            .unwrap();
        shuffle_btn.click();
        next_tick().await;

        // Clicking Shuffle must send a typed `team_member_shuffle` frame
        // targeting the dialog's team. The frontend never picks names,
        // agents, or personalities locally.
        let frames = recorded_frames(&calls);
        let shuffle_frame = frames
            .iter()
            .find(|(kind, _)| kind == &FrameKind::TeamMemberShuffle.to_string())
            .expect("shuffle click must dispatch TeamMemberShuffle");
        assert_eq!(
            shuffle_frame
                .1
                .get("team_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            "t-1",
            "shuffle frame should carry the team id: {:?}",
            shuffle_frame.1
        );

        // The form starts empty; the server-emitted suggestion notify is
        // what populates it.
        let name_input: web_sys::HtmlInputElement = container
            .query_selector_all("input[type='text']")
            .unwrap()
            .item(0)
            .unwrap()
            .dyn_into()
            .unwrap();
        let description_input: web_sys::HtmlInputElement = container
            .query_selector_all("input[type='text']")
            .unwrap()
            .item(1)
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            name_input.value().is_empty(),
            "name should remain empty until server suggestion arrives, got {:?}",
            name_input.value()
        );

        // Simulate the server-emitted notify by calling the dispatch helper
        // directly. The dialog's Effect should then apply the suggestion to
        // the form.
        state.record_team_member_shuffle_suggestion(
            host_id,
            TeamMemberShuffleSuggestionNotifyPayload {
                team_id: TeamId("t-1".to_owned()),
                suggestion: TeamMemberShuffleSuggestion {
                    name: "Server Picked Name".to_owned(),
                    description: "Server picked description.".to_owned(),
                    profile: TeamMemberPresetProfile {
                        role_preset_id: Some(TeamRolePresetId("frontend-specialist".to_owned())),
                        personality_preset_id: Some(TeamPersonalityPresetId(
                            "pragmatic-shipper".to_owned(),
                        )),
                        personality_traits: vec![TeamPersonalityTrait::Pragmatic],
                    },
                    custom_agent_id: Some(CustomAgentId("tyde-frontend-engineer".to_owned())),
                },
            },
        );
        next_tick().await;

        assert_eq!(
            name_input.value(),
            "Server Picked Name",
            "name input should reflect server-emitted suggestion"
        );
        assert_eq!(
            description_input.value(),
            "Server picked description.",
            "description input should reflect server-emitted suggestion"
        );
        let custom_agent_select =
            find_form_select(&container, "Custom agent").expect("custom agent select should exist");
        assert_eq!(
            custom_agent_select.value(),
            "tyde-frontend-engineer",
            "custom agent should reflect server-emitted suggestion"
        );
    }

    fn find_form_select(
        container: &HtmlElement,
        label_text: &str,
    ) -> Option<web_sys::HtmlSelectElement> {
        let labels = container
            .query_selector_all("label.settings-form-label")
            .unwrap();
        for i in 0..labels.length() {
            let label = labels.item(i).unwrap().dyn_into::<HtmlElement>().unwrap();
            let span_text = label
                .query_selector("span")
                .unwrap()
                .and_then(|s| s.text_content());
            if span_text.as_deref().map(str::trim) != Some(label_text) {
                continue;
            }
            return label
                .query_selector("select")
                .unwrap()
                .map(|el| el.dyn_into::<web_sys::HtmlSelectElement>().unwrap());
        }
        None
    }

    #[wasm_bindgen_test]
    async fn add_report_dialog_does_not_apply_stale_suggestion_on_reopen() {
        let _calls = install_send_stub();
        let container = make_container();
        let host_id = "host-shuffle-stale";
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager",
                TeamMemberRole::Manager,
            )],
        );
        install_host_stream(&state, host_id);
        install_project(&state, host_id, "p-1", "Test Project");
        install_catalog(&state, host_id);

        // A prior dialog session left a suggestion in state. Opening the
        // dialog must NOT auto-apply that stale suggestion — only fresh
        // notifies from a Shuffle click in this session should apply.
        state.record_team_member_shuffle_suggestion(
            host_id,
            TeamMemberShuffleSuggestionNotifyPayload {
                team_id: TeamId("t-1".to_owned()),
                suggestion: TeamMemberShuffleSuggestion {
                    name: "Stale Name".to_owned(),
                    description: "Stale description.".to_owned(),
                    profile: TeamMemberPresetProfile {
                        role_preset_id: Some(TeamRolePresetId("frontend-specialist".to_owned())),
                        personality_preset_id: None,
                        personality_traits: Vec::new(),
                    },
                    custom_agent_id: None,
                },
            },
        );

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ Report");
        next_tick().await;

        let name_input: web_sys::HtmlInputElement = container
            .query_selector_all("input[type='text']")
            .unwrap()
            .item(0)
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            name_input.value().is_empty(),
            "stale suggestion must not auto-apply on dialog open, got {:?}",
            name_input.value()
        );
    }

    #[wasm_bindgen_test]
    async fn add_report_edit_member_has_no_shuffle_button() {
        let _calls = install_send_stub();
        let container = make_container();
        let host_id = "host-edit-no-shuffle";
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![
                make_member("m-1", "t-1", "Manager", TeamMemberRole::Manager),
                make_member("m-2", "t-1", "Report One", TeamMemberRole::Report),
            ],
        );
        install_host_stream(&state, host_id);
        install_project(&state, host_id, "p-1", "Test Project");
        install_catalog(&state, host_id);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let edit_btn: HtmlElement = container
            .query_selector("button[aria-label='Edit member']")
            .unwrap()
            .expect("edit member icon button should be present")
            .dyn_into()
            .unwrap();
        edit_btn.click();
        next_tick().await;

        assert!(
            container
                .query_selector(".member-dialog-shuffle")
                .unwrap()
                .is_none(),
            "shuffle button should be hidden when editing an existing member"
        );
    }

    #[wasm_bindgen_test]
    async fn new_team_dialog_uses_wide_modal_for_draft_layout() {
        let _calls = install_send_stub();
        let container = make_container();
        let host_id = "host-new-team-wide";
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ New team");
        next_tick().await;

        let modal: HtmlElement = container
            .query_selector(".settings-confirm-modal")
            .unwrap()
            .expect("new-team modal should render")
            .dyn_into()
            .unwrap();
        let class_list = modal.class_name();
        assert!(
            class_list
                .split_whitespace()
                .any(|c| c == "settings-confirm-modal-wide"),
            "new-team modal must use the wide variant so the member grid has room: {class_list:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn generated_balanced_team_draft_fits_in_viewport() {
        ensure_styles_loaded();
        let _calls = install_send_stub();
        let container = make_container();
        let host_id = "host-balanced-fit";
        let state = install_state(host_id, vec![], vec![]);
        install_host_stream(&state, host_id);
        install_catalog(&state, host_id);

        // Install a draft that mirrors the balanced-team output: multiple
        // member cards that would previously overflow the viewport.
        let members = (0..6)
            .map(|i| {
                let id = format!("dm-{i}");
                let role = if i == 0 {
                    TeamMemberRole::Manager
                } else {
                    TeamMemberRole::Report
                };
                make_draft_member(&id, role, &format!("Member {i}"), &format!("Role {i}"))
            })
            .collect::<Vec<_>>();
        let draft = make_draft("Balanced Team", members);
        install_draft(&state, host_id, draft);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ New team");
        next_tick().await;

        let modal: HtmlElement = container
            .query_selector(".settings-confirm-modal")
            .unwrap()
            .expect("new-team modal should render the draft")
            .dyn_into()
            .unwrap();

        // Viewport height in this harness is 800px. The modal must not exceed
        // that height — otherwise commit/discard would be unreachable.
        let modal_rect = modal.get_bounding_client_rect();
        let viewport_height = web_sys::window()
            .unwrap()
            .inner_height()
            .unwrap()
            .as_f64()
            .unwrap();
        assert!(
            modal_rect.height() <= viewport_height,
            "modal height {} must fit viewport {}; member list should scroll internally",
            modal_rect.height(),
            viewport_height
        );

        // The commit footer must be visible — i.e. its bottom edge must be
        // inside the viewport. This is the user-perceived assertion: the
        // Create-team button is reachable.
        let commit_btn: HtmlElement = container
            .query_selector(".team-draft-commit")
            .unwrap()
            .expect("commit button should render")
            .dyn_into()
            .unwrap();
        let commit_rect = commit_btn.get_bounding_client_rect();
        assert!(
            commit_rect.bottom() <= viewport_height,
            "commit button bottom {} must be visible in viewport {}",
            commit_rect.bottom(),
            viewport_height
        );
    }

    #[wasm_bindgen_test]
    async fn add_report_dialog_requires_backend_before_sending() {
        let calls = install_send_stub();
        let container = make_container();
        let host_id = "host-backend-required";
        let project_id = "p-1";
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-1")],
            vec![make_member(
                "m-1",
                "t-1",
                "Manager",
                TeamMemberRole::Manager,
            )],
        );
        install_host_stream(&state, host_id);
        install_project(&state, host_id, project_id, "Test Project");

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        click_button_with_text(&container, "+ Report");
        next_tick().await;
        set_nth_text_input(&container, 0, "Backendless Report");
        next_tick().await;
        set_nth_text_input(&container, 1, "Needs a backend");
        next_tick().await;
        check_project_checkbox(&container, project_id);
        next_tick().await;
        click_button_with_text(&container, "Save");
        next_tick().await;

        let text = visible_text(&container);
        assert!(
            text.contains("Pick a backend."),
            "expected backend validation error: {text:?}"
        );
        assert!(
            recorded_frames(&calls).is_empty(),
            "validation should stop sends before any frames are written"
        );
    }

    /// Promote/set-manager affordance is the only per-member action that
    /// must stay out of the way until the user is engaging with a row.
    /// Hover/focus-within reveals it. We assert on the *rendered* opacity
    /// from the production stylesheet (not on a class name) so this test
    /// won't accept a future refactor that drops the visibility rule.
    #[wasm_bindgen_test]
    async fn promote_button_hidden_until_row_hovered_or_focused() {
        ensure_styles_loaded();
        let host_id = "host-promote";
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-mgr")],
            vec![
                make_member("m-mgr", "t-1", "Manager", TeamMemberRole::Manager),
                make_member("m-rep", "t-1", "Reporter", TeamMemberRole::Report),
            ],
        );
        install_host_stream(&state, host_id);
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let promote_btn: HtmlElement = container
            .query_selector(".team-member-icon-btn-promote")
            .unwrap()
            .expect("promote button must render in the DOM (visibility-only hidden)")
            .dyn_into()
            .unwrap();

        let row_for_promote = promote_btn
            .closest(".team-member-row")
            .unwrap()
            .expect("promote button must live inside a team-member-row")
            .dyn_into::<HtmlElement>()
            .unwrap();

        let window = web_sys::window().unwrap();
        // Baseline (no hover, no focus): the promote button is rendered
        // but visually hidden via opacity:0 + pointer-events:none. The
        // hidden state is what keeps it out of the way until the user
        // engages with the row.
        let baseline_style = window
            .get_computed_style(&promote_btn)
            .unwrap()
            .expect("computed style for promote button");
        assert_eq!(
            baseline_style.get_property_value("opacity").unwrap(),
            "0",
            "promote button must default to opacity 0 (hidden until hover/focus)"
        );
        assert_eq!(
            baseline_style.get_property_value("pointer-events").unwrap(),
            "none",
            "promote button must default to pointer-events:none so it can't be misclicked while hidden"
        );

        // Keyboard reveal: focusing the row's first reachable button
        // brings the promote button into view via :focus-within. This is
        // the accessibility guarantee — keyboard users must still be able
        // to reach the promote action.
        let edit_btn: HtmlElement = row_for_promote
            .query_selector(".team-member-icon-btn[aria-label='Edit member']")
            .unwrap()
            .expect("edit-member button should exist in the row")
            .dyn_into()
            .unwrap();
        edit_btn.focus().unwrap();
        next_tick().await;
        let focused_style = window
            .get_computed_style(&promote_btn)
            .unwrap()
            .expect("computed style after focus");
        assert_eq!(
            focused_style.get_property_value("opacity").unwrap(),
            "1",
            "promote button must become visible when keyboard focus enters the row (accessibility)"
        );
        assert_eq!(
            focused_style.get_property_value("pointer-events").unwrap(),
            "auto",
            "promote button must become clickable when keyboard focus enters the row"
        );
    }

    /// Team-level Compact button is enabled only when every Active member
    /// with a live binding is Idle and not already mid-compaction, and at
    /// least one member is bound. Clicking it (through the OK-stubbed
    /// confirm dialog) sends a single `TeamCompact` frame on the host
    /// stream — server fans out per-member compactions internally.
    /// While any member is Thinking the button is disabled; while
    /// compactions are in flight the button stays disabled to prevent
    /// double-fire.
    #[wasm_bindgen_test]
    async fn team_compact_button_gated_on_every_member_idle_and_sends_team_compact_frame() {
        let calls = install_send_stub();
        let _ = js_sys::eval(
            r#"
            window.__TAURI__.core.invoke = function(cmd, args) {
                window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                if (cmd === 'plugin:dialog|message') {
                    return Promise.resolve('Ok');
                }
                return Promise.resolve();
            };
            "#,
        );

        let host_id = "host-team-compact";
        let manager_id = TeamMemberId("m-mgr".to_owned());
        let report_id = TeamMemberId("m-rep".to_owned());
        let mgr_agent_id = AgentId("a-mgr".to_owned());
        let rep_agent_id = AgentId("a-rep".to_owned());
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-mgr")],
            vec![
                make_member("m-mgr", "t-1", "Manager", TeamMemberRole::Manager),
                make_member("m-rep", "t-1", "Reporter", TeamMemberRole::Report),
            ],
        );
        install_host_stream(&state, host_id);
        state.connection_statuses.update(|m| {
            m.insert(host_id.to_owned(), ConnectionStatus::Connected);
        });
        state.agents.update(|agents| {
            agents.push(crate::state::AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: mgr_agent_id.clone(),
                name: "Manager Agent".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/a-mgr/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            });
            agents.push(crate::state::AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: rep_agent_id.clone(),
                name: "Reporter Agent".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/a-rep/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            });
        });

        let container = make_container();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        // No bindings yet → the team has nothing to compact, so the
        // button is disabled. (We never hide the button to avoid CLS
        // and to keep the affordance discoverable.)
        let team_compact_btn = || -> Option<HtmlElement> {
            container
                .query_selector(".team-card-compact")
                .unwrap()
                .map(|el| el.dyn_into::<HtmlElement>().unwrap())
        };
        let btn = team_compact_btn().expect("team-card-compact button must render");
        assert!(
            btn.has_attribute("disabled"),
            "team Compact button must be disabled when no member is bound"
        );

        // Bind manager Idle, reporter Thinking. Mixed state must keep
        // the button disabled — must not imply the action is safe.
        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: Some(mgr_agent_id.clone()),
                    status: AgentControlStatus::Idle,
                    last_active_at_ms: Some(1),
                },
            );
            entry.insert(
                report_id.clone(),
                TeamMemberBindingPayload {
                    member_id: report_id.clone(),
                    current_agent_id: Some(rep_agent_id.clone()),
                    status: AgentControlStatus::Thinking,
                    last_active_at_ms: Some(2),
                },
            );
        });
        next_tick().await;
        let btn = team_compact_btn().expect("team Compact button must still render");
        assert!(
            btn.has_attribute("disabled"),
            "team Compact button must be disabled while any member is Thinking"
        );

        // Flip reporter to Idle. Now every bound member is Idle → enabled.
        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                report_id.clone(),
                TeamMemberBindingPayload {
                    member_id: report_id.clone(),
                    current_agent_id: Some(rep_agent_id.clone()),
                    status: AgentControlStatus::Idle,
                    last_active_at_ms: Some(3),
                },
            );
        });
        next_tick().await;
        let btn = team_compact_btn().expect("team Compact button must still render");
        assert!(
            !btn.has_attribute("disabled"),
            "team Compact button must be enabled when every bound member is Idle"
        );

        // Click → confirm → send a single TeamCompact frame on the
        // host stream. Server fans out internally.
        btn.click();
        for _ in 0..8 {
            next_tick().await;
        }
        let frames = recorded_frames(&calls);
        let team_compact_frames: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::TeamCompact.to_string())
            .collect();
        assert_eq!(
            team_compact_frames.len(),
            1,
            "team Compact must send exactly one TeamCompact frame, got frames: {frames:?}"
        );
        // Confirm the team_id in the payload + that we routed on the
        // host stream (not any agent stream).
        let team_compact_payload = team_compact_frames[0].1.clone();
        assert_eq!(
            team_compact_payload.get("team_id").and_then(|v| v.as_str()),
            Some("t-1"),
            "TeamCompact payload must carry the team_id, got: {team_compact_payload:?}"
        );
        let mut routed_to_host_stream = false;
        for entry in calls.iter() {
            let arr = entry.dyn_into::<js_sys::Array>().expect("array");
            if arr.get(0).as_string().as_deref() != Some("send_host_line") {
                continue;
            }
            let args_json = arr.get(1).as_string().expect("args");
            let args: JsonValue = serde_json::from_str(&args_json).expect("args parse");
            let line = args.get("line").and_then(|v| v.as_str()).expect("line");
            let env: JsonValue = serde_json::from_str(line).expect("envelope parse");
            if env.get("kind").and_then(|v| v.as_str()) == Some("team_compact") {
                routed_to_host_stream =
                    env.get("stream").and_then(|v| v.as_str()) == Some("/host/host-team-compact");
                break;
            }
        }
        assert!(
            routed_to_host_stream,
            "TeamCompact must target the host stream, not an agent instance stream"
        );

        // Both members are flipped to in-progress so the per-member
        // compact buttons hide and a second click on the team button
        // becomes a no-op (state guards).
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&mgr_agent_id)),
            "manager agent should be marked in-flight after team compact"
        );
        assert!(
            state
                .compaction_in_progress
                .with(|map| map.contains_key(&rep_agent_id)),
            "reporter agent should be marked in-flight after team compact"
        );
        // With both agents now in compaction_in_progress, gating must
        // re-disable the team Compact button — same defense the per-
        // member button has against double-fire.
        next_tick().await;
        let btn = team_compact_btn().expect("team Compact button must still render after click");
        assert!(
            btn.has_attribute("disabled"),
            "team Compact button must re-disable while any member is mid-compaction"
        );
    }

    /// Server rule: an Active member whose binding is not Idle blocks
    /// the entire team from compacting, *even if that member has no
    /// `current_agent_id`*. The frontend gating must match — otherwise
    /// the user would see an enabled button that the server then
    /// rejects with a 409. Also asserts that no `TeamCompact` frame is
    /// sent if the user somehow forces a click in this state.
    #[wasm_bindgen_test]
    async fn team_compact_disabled_when_active_member_unbound_but_not_idle() {
        let calls = install_send_stub();
        let _ = js_sys::eval(
            r#"
            window.__TAURI__.core.invoke = function(cmd, args) {
                window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                if (cmd === 'plugin:dialog|message') {
                    return Promise.resolve('Ok');
                }
                return Promise.resolve();
            };
            "#,
        );

        let host_id = "host-team-compact-unbound";
        let manager_id = TeamMemberId("m-mgr".to_owned());
        let report_id = TeamMemberId("m-rep".to_owned());
        let mgr_agent_id = AgentId("a-mgr".to_owned());
        let state = install_state(
            host_id,
            vec![make_team("t-1", "Alpha", "m-mgr")],
            vec![
                make_member("m-mgr", "t-1", "Manager", TeamMemberRole::Manager),
                make_member("m-rep", "t-1", "Reporter", TeamMemberRole::Report),
            ],
        );
        install_host_stream(&state, host_id);
        state.connection_statuses.update(|m| {
            m.insert(host_id.to_owned(), ConnectionStatus::Connected);
        });
        state.agents.update(|agents| {
            agents.push(crate::state::AgentInfo {
                host_id: host_id.to_owned(),
                agent_id: mgr_agent_id.clone(),
                name: "Manager Agent".to_owned(),
                origin: protocol::AgentOrigin::User,
                backend_kind: protocol::BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                workflow: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/a-mgr/inst".to_owned()),
                started: true,
                fatal_error: None,
                activity_summary: Default::default(),
            });
        });
        // Manager: bound, Idle (a valid target on its own).
        // Reporter: bound but UNBOUND-from-agent (current_agent_id =
        // None) and status Thinking. This is the case the server
        // explicitly rejects: not-Idle gates the whole team, even
        // without a live agent.
        state.team_member_bindings.update(|m| {
            let entry = m.entry(host_id.to_owned()).or_default();
            entry.insert(
                manager_id.clone(),
                TeamMemberBindingPayload {
                    member_id: manager_id.clone(),
                    current_agent_id: Some(mgr_agent_id.clone()),
                    status: AgentControlStatus::Idle,
                    last_active_at_ms: Some(1),
                },
            );
            entry.insert(
                report_id.clone(),
                TeamMemberBindingPayload {
                    member_id: report_id.clone(),
                    current_agent_id: None,
                    status: AgentControlStatus::Thinking,
                    last_active_at_ms: Some(2),
                },
            );
        });

        let container = make_container();
        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <TeamsPanel /> }
        });
        next_tick().await;

        let team_compact_btn = || -> Option<HtmlElement> {
            container
                .query_selector(".team-card-compact")
                .unwrap()
                .map(|el| el.dyn_into::<HtmlElement>().unwrap())
        };
        let btn = team_compact_btn().expect("team-card-compact button must render");
        assert!(
            btn.has_attribute("disabled"),
            "team Compact must be disabled when any Active member binding is non-Idle, \
             even if unbound — matches server reject"
        );

        // Defense in depth: even forcing a click does not send a frame
        // (the click handler short-circuits when `team_compact_targets`
        // returns None).
        btn.click();
        for _ in 0..4 {
            next_tick().await;
        }
        let frames = recorded_frames(&calls);
        let team_compact_frames: Vec<_> = frames
            .iter()
            .filter(|(kind, _)| kind == &FrameKind::TeamCompact.to_string())
            .collect();
        assert!(
            team_compact_frames.is_empty(),
            "no TeamCompact frame may leave the client while gating disallows the action, got: {frames:?}"
        );
    }
}
