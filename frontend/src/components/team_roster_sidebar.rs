use leptos::prelude::*;

use protocol::{
    AgentControlStatus, TeamId, TeamMember, TeamMemberId, TeamMemberRole, TeamMemberState,
};

use crate::components::teams_panel::open_member_chat;
use crate::state::AppState;

/// Roster sidebar that mounts alongside a manager's chat view (see
/// `dev-docs/19-agent-teams.md` §9.2). Lists each report's name, role badge,
/// `CustomAgent` label, bound projects, live-binding status, and last-active
/// timestamp. Clicking a report routes through the same
/// `open_member_chat` 3-state activation flow used by the Teams panel.
#[component]
pub fn TeamRosterSidebar(host_id: String, team_id: TeamId) -> impl IntoView {
    let state = expect_context::<AppState>();

    let team_id_for_name = team_id.clone();
    let host_for_name = host_id.clone();
    let state_for_name = state.clone();
    let team_name = move || {
        state_for_name.teams.with(|map| {
            map.get(&host_for_name)
                .and_then(|m| m.get(&team_id_for_name))
                .map(|t| t.name.clone())
                .unwrap_or_default()
        })
    };

    let team_id_for_reports = team_id.clone();
    let host_for_reports = host_id.clone();
    let state_for_reports = state.clone();
    let reports: Memo<Vec<TeamMember>> = Memo::new(move |_| {
        let mut members: Vec<TeamMember> = state_for_reports.team_members.with(|map| {
            map.get(&host_for_reports)
                .map(|m| {
                    m.values()
                        .filter(|member| {
                            member.team_id == team_id_for_reports
                                && member.role == TeamMemberRole::Report
                                && member.state == TeamMemberState::Active
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        });
        members.sort_by(|a, b| a.name.cmp(&b.name));
        members
    });

    view! {
        <aside class="team-roster-sidebar" aria-label="Team roster">
            <div class="team-roster-header">
                <span class="team-roster-title">{team_name}</span>
            </div>
            <div class="team-roster-list">
                <For
                    each=move || reports.get()
                    key=|member| member.id.clone()
                    let:report
                >
                    {
                        let host = host_id.clone();
                        let report_id = report.id.clone();
                        view! {
                            <RosterReportRow host_id=host member_id=report_id />
                        }
                    }
                </For>
                {move || reports.get().is_empty().then(|| view! {
                    <div class="team-roster-empty">"No reports yet"</div>
                })}
            </div>
        </aside>
    }
}

#[component]
fn RosterReportRow(host_id: String, member_id: TeamMemberId) -> impl IntoView {
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

    let host_for_agent = host_id.clone();
    let state_for_agent = state.clone();
    let custom_agent_label = move || -> Option<String> {
        let m = member.get()?;
        state_for_agent.custom_agents.with(|map| {
            map.get(&host_for_agent)
                .and_then(|m2| m2.get(&m.custom_agent_id).map(|c| c.name.clone()))
        })
    };

    let host_for_project = host_id.clone();
    let state_for_project = state.clone();
    let project_label = move || -> Option<String> {
        let m = member.get()?;
        if m.project_ids.is_empty() {
            return None;
        }
        let host = host_for_project.clone();
        let names = state_for_project.projects.with(|projects| {
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
        });
        Some(names)
    };

    let mid_for_binding = member_id.clone();
    let host_for_binding = host_id.clone();
    let state_for_binding = state.clone();
    let binding_status = move || -> Option<AgentControlStatus> {
        state_for_binding.team_member_bindings.with(|map| {
            map.get(&host_for_binding)
                .and_then(|m| m.get(&mid_for_binding))
                .map(|b| b.status)
        })
    };

    let mid_for_active = member_id.clone();
    let host_for_active = host_id.clone();
    let state_for_active = state.clone();
    let last_active_label = move || -> Option<String> {
        let last_active = state_for_active.team_member_bindings.with(|map| {
            map.get(&host_for_active)
                .and_then(|m| m.get(&mid_for_active))
                .and_then(|b| b.last_active_at_ms)
        })?;
        Some(format!("active {last_active}"))
    };

    let mid_for_click = member_id;
    let host_for_click = host_id.clone();
    let state_for_click = state.clone();
    let on_click = move |_: web_sys::MouseEvent| {
        open_member_chat(
            &state_for_click,
            host_for_click.clone(),
            mid_for_click.clone(),
        );
    };

    view! {
        <div class="team-roster-row" role="button" tabindex="0" on:click=on_click>
            <div class="team-roster-row-line">
                <span class="team-roster-row-name">
                    {move || member.get().map(|m| m.name).unwrap_or_default()}
                </span>
                <span class="team-roster-row-role">"Report"</span>
                {move || binding_status().map(|status| view! {
                    <span class="team-roster-row-status">
                        {format!("{status:?}").to_lowercase()}
                    </span>
                })}
            </div>
            <div class="team-roster-row-meta">
                {move || custom_agent_label().map(|s| view! {
                    <span class="team-roster-row-custom-agent">{s}</span>
                })}
                {move || project_label().map(|s| view! {
                    <span class="team-roster-row-project">{s}</span>
                })}
                {move || last_active_label().map(|s| view! {
                    <span class="team-roster-row-active">{s}</span>
                })}
            </div>
        </div>
    }
}
