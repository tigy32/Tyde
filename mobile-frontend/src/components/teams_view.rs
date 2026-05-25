use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone, StatusDot, StatusTone,
};
use crate::state::{AppState, LocalHostId};

/// Per-host teams roster. Surfaced under the Agents tab via a
/// segmented Agents/Teams toggle. Each team renders a Card with its
/// members (manager flagged + reports listed), live binding status
/// dots, an open-chat affordance per member, and a Compact-team
/// destructive action.
#[component]
pub fn TeamsView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    view! {
        <div class="teams-view" data-mobile-test="teams-view">
            <div class="view-body">
                {move || {
                    let Some(host) = state.active_local_host_id.get() else {
                        return view! {
                            <EmptyState
                                title="No host selected"
                                body="Pick a host to see its teams."
                                icon="\u{1F517}"
                                data_mobile_test="teams-no-host"
                            />
                        }.into_any();
                    };
                    let teams = state
                        .teams_by_host
                        .with(|m| m.get(&host).cloned())
                        .unwrap_or_default();
                    if teams.is_empty() {
                        return view! {
                            <EmptyState
                                title="No teams"
                                body="Teams created on desktop will show up here so you can browse members and trigger compactions on the go."
                                icon="\u{1F465}"
                                data_mobile_test="teams-empty"
                            />
                        }.into_any();
                    }
                    let mut sorted: Vec<_> = teams.into_values().collect();
                    sorted.sort_by(|a, b| a.name.cmp(&b.name));
                    view! {
                        <div class="teams-list" data-mobile-test="teams-list">
                            {sorted.into_iter().map(|team| {
                                view! { <TeamCard team=team host=host.clone() /> }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </div>
        </div>
    }
}

#[component]
fn TeamCard(team: protocol::Team, host: LocalHostId) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let team_id = team.id.clone();

    let team_id_for_members = team_id.clone();
    let host_for_members = host.clone();
    let members = move || {
        let inner = state
            .team_members_by_host
            .with(|m| m.get(&host_for_members).cloned())
            .unwrap_or_default();
        let mut rows: Vec<_> = inner
            .into_values()
            .filter(|m| m.team_id == team_id_for_members)
            .collect();
        rows.sort_by(|a, b| {
            // Manager first, then report; then by name.
            let a_mgr = matches!(a.role, protocol::TeamMemberRole::Manager);
            let b_mgr = matches!(b.role, protocol::TeamMemberRole::Manager);
            b_mgr.cmp(&a_mgr).then_with(|| a.name.cmp(&b.name))
        });
        rows
    };

    let team_id_for_bindings = team.id.clone();
    let host_for_bindings = host.clone();
    let binding_for = move |member_id: &protocol::TeamMemberId| {
        let _ = &team_id_for_bindings;
        state.team_bindings_by_host.with(|m| {
            m.get(&host_for_bindings)
                .and_then(|inner| inner.get(member_id).cloned())
        })
    };

    let team_id_for_compact = team.id.clone();
    let host_for_compact = host.clone();
    let state_for_compact = state.clone();
    let on_compact = Callback::new(move |_: ()| {
        let host = host_for_compact.clone();
        let team_id = team_id_for_compact.clone();
        let state = state_for_compact.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::compact_team(&state, &host, team_id, None, None).await {
                log::error!("compact_team failed: {e}");
            }
        });
    });

    let team_id_for_compact_pill = team.id.clone();
    let host_for_compact_pill = host.clone();
    let compaction_status = move || {
        state.team_compactions_by_host.with(|m| {
            m.get(&host_for_compact_pill)
                .and_then(|inner| inner.get(&team_id_for_compact_pill).cloned())
        })
    };

    view! {
        <Card data_mobile_test="teams-row" dense=true>
            <div class="teams-row-header">
                <div class="teams-row-title">{team.name}</div>
                {move || {
                    compaction_status().map(|c| {
                        let (label, tone) = match c.status {
                            protocol::TeamCompactStatus::Started => ("Compacting…", PillTone::Accent),
                            protocol::TeamCompactStatus::Completed => ("Compacted", PillTone::Success),
                            protocol::TeamCompactStatus::Failed => ("Compaction failed", PillTone::Error),
                        };
                        view! {
                            <Pill label=label tone=tone data_mobile_test="teams-row-compaction" />
                        }
                    })
                }}
                <Button
                    label="Compact"
                    variant=ButtonVariant::Destructive
                    size=ButtonSize::Compact
                    data_mobile_test="teams-row-compact-button"
                    on_click=on_compact
                />
            </div>
            <div class="teams-members-list" data-mobile-test="teams-members-list">
                {move || {
                    let host_for_member = host.clone();
                    members().into_iter().map(|member| {
                        let binding = binding_for(&member.id);
                        let host = host_for_member.clone();
                        view! { <TeamMemberRow member=member binding=binding host=host /> }
                    }).collect::<Vec<_>>()
                }}
            </div>
        </Card>
    }
}

#[component]
fn TeamMemberRow(
    member: protocol::TeamMember,
    binding: Option<protocol::TeamMemberBindingPayload>,
    host: LocalHostId,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let role_label = match member.role {
        protocol::TeamMemberRole::Manager => "Manager",
        protocol::TeamMemberRole::Report => "Report",
    };
    let backend = format!("{:?}", member.backend_kind);
    let (tone, status_label) = match binding.as_ref().map(|b| b.status) {
        Some(protocol::AgentControlStatus::Thinking) => (StatusTone::Active, "Thinking"),
        Some(protocol::AgentControlStatus::Idle) => (StatusTone::Online, "Idle"),
        Some(protocol::AgentControlStatus::Failed) => (StatusTone::Error, "Failed"),
        None => (StatusTone::Muted, "No session"),
    };
    let member_id = member.id.clone();
    let member_id_for_activate = member_id.clone();
    let host_for_activate = host.clone();
    let state_for_activate = state.clone();
    let on_activate = Callback::new(move |_: ()| {
        let host = host_for_activate.clone();
        let id = member_id_for_activate.clone();
        let state = state_for_activate.clone();
        spawn_local(async move {
            if let Err(e) =
                crate::actions::activate_team_member(&state, &host, id, None, None).await
            {
                log::error!("activate_team_member failed: {e}");
            }
        });
    });
    view! {
        <div class="teams-member-row" data-mobile-test="teams-member-row">
            <div class="teams-member-row-header">
                <StatusDot tone=tone label=status_label.to_string() />
                <span class="teams-member-name" data-mobile-test="teams-member-name">{member.name}</span>
                <Pill label=role_label tone=match member.role {
                    protocol::TeamMemberRole::Manager => PillTone::Accent,
                    protocol::TeamMemberRole::Report => PillTone::Neutral,
                } data_mobile_test="teams-member-role" />
                <Pill label=backend tone=PillTone::Neutral />
            </div>
            <div class="teams-member-row-description">{member.description}</div>
            <div class="teams-member-row-actions">
                <Button
                    label="Open chat"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="teams-member-open-chat"
                    on_click=on_activate
                />
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{
        BackendKind, Team, TeamId, TeamMember, TeamMemberBindingPayload, TeamMemberId,
        TeamMemberRole, TeamMemberState,
    };
    use std::collections::HashMap;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

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

    fn fixture_team(host: &LocalHostId) -> (Team, TeamMember, TeamMember) {
        let team_id = TeamId("t1".to_owned());
        let mgr_id = TeamMemberId("m1".to_owned());
        let mgr = TeamMember {
            id: mgr_id.clone(),
            team_id: team_id.clone(),
            role: TeamMemberRole::Manager,
            state: TeamMemberState::Active,
            name: "Lead".to_owned(),
            description: "Coordinates the team".to_owned(),
            profile: None,
            custom_agent_id: None,
            backend_kind: BackendKind::Claude,
            cost_hint: None,
            session_id: None,
            project_ids: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let rep = TeamMember {
            id: TeamMemberId("m2".to_owned()),
            team_id: team_id.clone(),
            role: TeamMemberRole::Report,
            state: TeamMemberState::Active,
            name: "Worker".to_owned(),
            description: "Does the work".to_owned(),
            profile: None,
            custom_agent_id: None,
            backend_kind: BackendKind::Claude,
            cost_hint: None,
            session_id: None,
            project_ids: Vec::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let team = Team {
            id: team_id,
            name: "Cool Team".to_owned(),
            manager_member_id: mgr_id,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let _ = host; // host is fixture-parameterized
        (team, mgr, rep)
    }

    #[wasm_bindgen_test]
    async fn teams_view_empty_state_when_no_teams() {
        let host = LocalHostId("h1".to_owned());
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host.clone()));
            provide_context(state);
            view! { <TeamsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='teams-empty']")
                .unwrap()
                .is_some()
        );
    }

    #[wasm_bindgen_test]
    async fn teams_view_renders_team_with_members_and_binding_status() {
        let host = LocalHostId("h1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            let (team, mgr, rep) = fixture_team(&host_for_mount);
            let mut team_map = HashMap::new();
            team_map.insert(team.id.clone(), team);
            state.teams_by_host.update(|m| {
                m.insert(host_for_mount.clone(), team_map);
            });
            let mut members = HashMap::new();
            members.insert(mgr.id.clone(), mgr.clone());
            members.insert(rep.id.clone(), rep.clone());
            state.team_members_by_host.update(|m| {
                m.insert(host_for_mount.clone(), members);
            });
            let mut bindings = HashMap::new();
            bindings.insert(
                mgr.id.clone(),
                TeamMemberBindingPayload {
                    member_id: mgr.id.clone(),
                    current_agent_id: None,
                    status: protocol::AgentControlStatus::Thinking,
                    last_active_at_ms: None,
                },
            );
            state.team_bindings_by_host.update(|m| {
                m.insert(host_for_mount.clone(), bindings);
            });
            provide_context(state);
            view! { <TeamsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='teams-row']")
                .unwrap()
                .is_some(),
            "team row must render"
        );
        // Members list must contain the manager and the report.
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Lead"), "manager name must render: {text}");
        assert!(text.contains("Worker"), "report name must render: {text}");
        assert!(text.contains("Manager"), "manager role pill must render");
        assert!(text.contains("Report"), "report role pill must render");
    }
}
