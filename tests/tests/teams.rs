mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentControlStatus, AgentId, BackendKind, CommandErrorCode, CommandErrorPayload, CustomAgent,
    CustomAgentDeletePayload, CustomAgentId, CustomAgentNotifyPayload, CustomAgentUpsertPayload,
    Envelope, FrameKind, HostSettingValue, NewAgentPayload, Project, ProjectCreatePayload,
    ProjectDeletePayload, ProjectNotifyPayload, SessionSettingValue, SessionSettingsPayload,
    SetSettingPayload, SpawnCostHint, StreamPath, Team, TeamCreatePayload, TeamDeletePayload,
    TeamDraft, TeamDraftApplyTemplatePayload, TeamDraftCommitPayload, TeamDraftCreatePayload,
    TeamDraftId, TeamDraftMember, TeamDraftNotifyPayload, TeamDraftShufflePayload,
    TeamDraftShuffleScope, TeamDraftUpdatePayload, TeamId, TeamMember, TeamMemberActivatePayload,
    TeamMemberBindingNotifyPayload, TeamMemberBindingPayload, TeamMemberCreatePayload,
    TeamMemberCreateSpec, TeamMemberDeletePayload, TeamMemberId, TeamMemberNotifyPayload,
    TeamMemberPresetProfile, TeamMemberRole, TeamMemberState, TeamNotifyPayload,
    TeamPersonalityPresetId, TeamPersonalityTrait, TeamPresetCatalogNotifyPayload,
    TeamRenamePayload, TeamRolePresetId, TeamSetManagerPayload, TeamTemplateId, ToolPolicy,
    write_envelope,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::{Value, json};

async fn next_env(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(10), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = next_env(client, context).await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::SessionList
                | FrameKind::ProjectFileList
                | FrameKind::ProjectGitStatus
                | FrameKind::TeamPresetCatalogNotify
        ) {
            continue;
        }
        return env;
    }
}

async fn expect_kind(client: &mut client::Connection, kind: FrameKind, context: &str) -> Envelope {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == kind {
            return env;
        }
    }
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    let env = expect_kind(client, FrameKind::CommandError, context).await;
    env.parse_payload().expect("parse CommandErrorPayload")
}

async fn expect_project_notify(client: &mut client::Connection, context: &str) -> Project {
    let env = expect_kind(client, FrameKind::ProjectNotify, context).await;
    match env
        .parse_payload::<ProjectNotifyPayload>()
        .expect("parse ProjectNotifyPayload")
    {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected ProjectNotify::Upsert, got {other:?}"),
    }
}

async fn expect_custom_agent_notify(
    client: &mut client::Connection,
    context: &str,
) -> CustomAgentNotifyPayload {
    let env = expect_kind(client, FrameKind::CustomAgentNotify, context).await;
    env.parse_payload().expect("parse CustomAgentNotifyPayload")
}

async fn expect_team_notify(client: &mut client::Connection, context: &str) -> Team {
    let env = expect_kind(client, FrameKind::TeamNotify, context).await;
    match env
        .parse_payload::<TeamNotifyPayload>()
        .expect("parse TeamNotifyPayload")
    {
        TeamNotifyPayload::Upsert { team } => team,
        other => panic!("expected TeamNotify::Upsert, got {other:?}"),
    }
}

async fn expect_team_delete_notify(client: &mut client::Connection, context: &str) -> Team {
    let env = expect_kind(client, FrameKind::TeamNotify, context).await;
    match env
        .parse_payload::<TeamNotifyPayload>()
        .expect("parse TeamNotifyPayload")
    {
        TeamNotifyPayload::Delete { team } => team,
        other => panic!("expected TeamNotify::Delete, got {other:?}"),
    }
}

async fn expect_team_member_notify(client: &mut client::Connection, context: &str) -> TeamMember {
    let env = expect_kind(client, FrameKind::TeamMemberNotify, context).await;
    match env
        .parse_payload::<TeamMemberNotifyPayload>()
        .expect("parse TeamMemberNotifyPayload")
    {
        TeamMemberNotifyPayload::Upsert { member } => member,
        other => panic!("expected TeamMemberNotify::Upsert, got {other:?}"),
    }
}

async fn expect_team_member_delete_notify(
    client: &mut client::Connection,
    context: &str,
) -> TeamMember {
    let env = expect_kind(client, FrameKind::TeamMemberNotify, context).await;
    match env
        .parse_payload::<TeamMemberNotifyPayload>()
        .expect("parse TeamMemberNotifyPayload")
    {
        TeamMemberNotifyPayload::Delete { member } => member,
        other => panic!("expected TeamMemberNotify::Delete, got {other:?}"),
    }
}

async fn expect_team_member_binding_notify(
    client: &mut client::Connection,
    context: &str,
) -> TeamMemberBindingPayload {
    let env = expect_kind(client, FrameKind::TeamMemberBindingNotify, context).await;
    match env
        .parse_payload::<TeamMemberBindingNotifyPayload>()
        .expect("parse TeamMemberBindingNotifyPayload")
    {
        TeamMemberBindingNotifyPayload::Upsert { binding } => binding,
        other => panic!("expected TeamMemberBindingNotify::Upsert, got {other:?}"),
    }
}

async fn expect_team_member_binding_delete_notify(
    client: &mut client::Connection,
    context: &str,
) -> TeamMemberBindingPayload {
    let env = expect_kind(client, FrameKind::TeamMemberBindingNotify, context).await;
    match env
        .parse_payload::<TeamMemberBindingNotifyPayload>()
        .expect("parse TeamMemberBindingNotifyPayload")
    {
        TeamMemberBindingNotifyPayload::Delete { binding } => binding,
        other => panic!("expected TeamMemberBindingNotify::Delete, got {other:?}"),
    }
}

async fn expect_team_draft_notify(client: &mut client::Connection, context: &str) -> TeamDraft {
    let env = expect_kind(client, FrameKind::TeamDraftNotify, context).await;
    match env
        .parse_payload::<TeamDraftNotifyPayload>()
        .expect("parse TeamDraftNotifyPayload")
    {
        TeamDraftNotifyPayload::Upsert { draft } => draft,
        other => panic!("expected TeamDraftNotify::Upsert, got {other:?}"),
    }
}

async fn expect_team_draft_delete_notify(
    client: &mut client::Connection,
    context: &str,
) -> TeamDraftId {
    let env = expect_kind(client, FrameKind::TeamDraftNotify, context).await;
    match env
        .parse_payload::<TeamDraftNotifyPayload>()
        .expect("parse TeamDraftNotifyPayload")
    {
        TeamDraftNotifyPayload::Delete { draft_id } => draft_id,
        other => panic!("expected TeamDraftNotify::Delete, got {other:?}"),
    }
}

async fn expect_team_catalog_notify(
    client: &mut client::Connection,
    context: &str,
) -> TeamPresetCatalogNotifyPayload {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::TeamPresetCatalogNotify {
            return env
                .parse_payload()
                .expect("parse TeamPresetCatalogNotifyPayload");
        }
    }
}

async fn expect_session_settings_on_stream(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> SessionSettingsPayload {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::SessionSettings && &env.stream == stream {
            return env.parse_payload().expect("parse SessionSettingsPayload");
        }
    }
}

async fn expect_bound_team_member(
    client: &mut client::Connection,
    member_id: &TeamMemberId,
    agent_id: &AgentId,
    context: &str,
) -> TeamMemberBindingPayload {
    loop {
        let binding = expect_team_member_binding_notify(client, context).await;
        if &binding.member_id == member_id && binding.current_agent_id.as_ref() == Some(agent_id) {
            return binding;
        }
    }
}

async fn call_agent_control_tool_json(
    fixture: &Fixture,
    agent_id: &AgentId,
    name: &str,
    arguments: Value,
) -> Value {
    let base_url = fixture.agent_control_http_url().await;
    let separator = if base_url.contains('?') { '&' } else { '?' };
    let url = format!("{base_url}{separator}agent_id={}", agent_id.0);
    let transport = StreamableHttpClientTransport::from_uri(url);
    let service = ().serve(transport).await.expect("connect to agent MCP");
    let result = service
        .call_tool(CallToolRequestParams {
            meta: None,
            name: name.to_string().into(),
            arguments: arguments.as_object().cloned(),
            task: None,
        })
        .await
        .expect("call agent-control tool");
    assert_eq!(
        result.is_error,
        Some(false),
        "agent-control tool returned error: {result:?}"
    );
    let content = result
        .content
        .first()
        .expect("tool result should include content");
    let RawContent::Text(text) = &content.raw else {
        panic!("expected text JSON tool result, got {:?}", content.raw);
    };
    let value = serde_json::from_str(&text.text).expect("tool result text must be JSON");
    service.cancel().await.expect("cancel MCP client");
    value
}

fn sample_custom_agent(id: &str) -> CustomAgent {
    CustomAgent {
        id: CustomAgentId(id.to_owned()),
        name: format!("{id} agent"),
        description: format!("{id} team agent"),
        instructions: Some(format!("{id} instructions")),
        skill_ids: Vec::new(),
        mcp_server_ids: Vec::new(),
        tool_policy: ToolPolicy::Unrestricted,
    }
}

fn member_spec(
    name: &str,
    custom_agent_id: Option<CustomAgentId>,
    project_ids: Vec<protocol::ProjectId>,
) -> TeamMemberCreateSpec {
    member_spec_with_profile(
        name,
        custom_agent_id,
        BackendKind::Claude,
        None,
        project_ids,
    )
}

fn member_spec_with_profile(
    name: &str,
    custom_agent_id: Option<CustomAgentId>,
    backend_kind: BackendKind,
    cost_hint: Option<SpawnCostHint>,
    project_ids: Vec<protocol::ProjectId>,
) -> TeamMemberCreateSpec {
    TeamMemberCreateSpec {
        name: name.to_owned(),
        description: format!("{name} description"),
        profile: None,
        custom_agent_id,
        backend_kind,
        cost_hint,
        project_ids,
    }
}

async fn upsert_custom_agent(client: &mut client::Connection, id: &str) -> CustomAgent {
    let custom_agent = sample_custom_agent(id);
    client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    assert_eq!(
        expect_custom_agent_notify(client, "CustomAgentNotify upsert").await,
        CustomAgentNotifyPayload::Upsert {
            custom_agent: custom_agent.clone()
        }
    );
    custom_agent
}

async fn create_project(client: &mut client::Connection, name: &str) -> Project {
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude, BackendKind::Codex],
            },
        })
        .await
        .expect("set enabled backends failed");
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots: vec![format!("/tmp/tyde-team-project-{name}")],
        })
        .await
        .expect("project_create failed");
    expect_project_notify(client, "ProjectNotify upsert").await
}

fn complete_draft_member(
    member: TeamDraftMember,
    backend_kind: BackendKind,
    project_id: protocol::ProjectId,
) -> protocol::TeamDraftMemberEdit {
    let name = if member.name.trim().is_empty() {
        "Manual Member".to_owned()
    } else {
        member.name
    };
    let description = if member.description.trim().is_empty() {
        "Manual member description".to_owned()
    } else {
        member.description
    };
    protocol::TeamDraftMemberEdit {
        id: member.id,
        name,
        description,
        custom_agent_id: member.custom_agent_id,
        backend_kind: Some(backend_kind),
        cost_hint: member.cost_hint,
        project_ids: vec![project_id],
    }
}

async fn create_team(
    client: &mut client::Connection,
    name: &str,
    custom_agent_id: CustomAgentId,
    project_id: Option<protocol::ProjectId>,
) -> (Team, TeamMember) {
    let project_ids = match project_id {
        Some(project_id) => vec![project_id],
        None => vec![
            create_project(client, &format!("{name}-manager-project"))
                .await
                .id,
        ],
    };
    client
        .team_create(TeamCreatePayload {
            name: name.to_owned(),
            manager: member_spec("manager", Some(custom_agent_id), project_ids),
        })
        .await
        .expect("team_create failed");
    let team = expect_team_notify(client, "TeamNotify create").await;
    let manager = expect_team_member_notify(client, "TeamMemberNotify manager create").await;
    let binding =
        expect_team_member_binding_notify(client, "TeamMemberBindingNotify manager create").await;
    assert_eq!(manager.id, team.manager_member_id);
    assert_eq!(binding.member_id, manager.id);
    assert_eq!(binding.current_agent_id, None);
    assert_eq!(binding.status, AgentControlStatus::Idle);
    (team, manager)
}

async fn create_report(
    client: &mut client::Connection,
    team_id: TeamId,
    name: &str,
    custom_agent_id: CustomAgentId,
    project_id: Option<protocol::ProjectId>,
) -> TeamMember {
    let project_ids = match project_id {
        Some(project_id) => vec![project_id],
        None => vec![
            create_project(client, &format!("{name}-report-project"))
                .await
                .id,
        ],
    };
    client
        .team_member_create(TeamMemberCreatePayload {
            team_id,
            member: member_spec(name, Some(custom_agent_id), project_ids),
            session_id: None,
        })
        .await
        .expect("team_member_create failed");
    let member = expect_team_member_notify(client, "TeamMemberNotify report create").await;
    let binding =
        expect_team_member_binding_notify(client, "TeamMemberBindingNotify report create").await;
    assert_eq!(binding.member_id, member.id);
    assert_eq!(binding.current_agent_id, None);
    assert_eq!(binding.status, AgentControlStatus::Idle);
    member
}

async fn create_team_with_report(
    fixture: &mut Fixture,
    unique: &str,
) -> (CustomAgent, Project, Team, TeamMember, TeamMember) {
    let custom_agent = upsert_custom_agent(&mut fixture.client, &format!("{unique}-agent")).await;
    let project = create_project(&mut fixture.client, &format!("{unique}-project")).await;
    let (team, manager) = create_team(
        &mut fixture.client,
        &format!("{unique} team"),
        custom_agent.id.clone(),
        Some(project.id.clone()),
    )
    .await;
    let report = create_report(
        &mut fixture.client,
        team.id.clone(),
        "report",
        custom_agent.id.clone(),
        Some(project.id.clone()),
    )
    .await;
    (custom_agent, project, team, manager, report)
}

async fn create_team_with_distinct_report_agent(
    fixture: &mut Fixture,
    unique: &str,
) -> (CustomAgent, CustomAgent, TeamMember) {
    let manager_agent =
        upsert_custom_agent(&mut fixture.client, &format!("{unique}-manager")).await;
    let report_agent = upsert_custom_agent(&mut fixture.client, &format!("{unique}-report")).await;
    let (team, _) = create_team(
        &mut fixture.client,
        &format!("{unique} team"),
        manager_agent.id.clone(),
        None,
    )
    .await;
    let report = create_report(
        &mut fixture.client,
        team.id,
        "report",
        report_agent.id.clone(),
        None,
    )
    .await;
    (manager_agent, report_agent, report)
}

fn host_stream(client: &client::Connection) -> protocol::StreamPath {
    let mut host_streams = client
        .outgoing_seq
        .keys()
        .filter(|stream| stream.0.starts_with("/host/"));
    let host_stream = host_streams.next().cloned().expect("missing host stream");
    assert!(
        host_streams.next().is_none(),
        "connection has multiple host streams"
    );
    host_stream
}

async fn send_raw_host_value(client: &mut client::Connection, kind: FrameKind, payload: Value) {
    let stream = host_stream(client);
    let seq = client
        .outgoing_seq
        .get(&stream)
        .copied()
        .expect("missing host stream sequence");
    let envelope = Envelope {
        stream: stream.clone(),
        kind,
        seq,
        payload,
    };
    client.outgoing_seq.insert(stream, seq + 1);
    write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write raw host envelope");
}

#[tokio::test]
async fn team_creation_round_trip_and_replay_order() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "round-trip-agent").await;
    let project = create_project(&mut fixture.client, "round-trip-project").await;
    let manager_spec = member_spec(
        "manager",
        Some(custom_agent.id.clone()),
        vec![project.id.clone()],
    );

    fixture
        .client
        .team_create(TeamCreatePayload {
            name: "Round Trip Team".to_owned(),
            manager: manager_spec.clone(),
        })
        .await
        .expect("team_create failed");

    let team = expect_team_notify(&mut fixture.client, "TeamNotify create").await;
    assert_eq!(team.name, "Round Trip Team");

    let manager = expect_team_member_notify(&mut fixture.client, "TeamMemberNotify manager").await;
    assert_eq!(manager.id, team.manager_member_id);
    assert_eq!(manager.team_id, team.id);
    assert_eq!(manager.role, TeamMemberRole::Manager);
    assert_eq!(manager.state, TeamMemberState::Active);
    assert_eq!(manager.name, manager_spec.name);
    assert_eq!(manager.description, manager_spec.description);
    assert_eq!(manager.custom_agent_id, Some(custom_agent.id.clone()));
    assert_eq!(manager.backend_kind, manager_spec.backend_kind);
    assert_eq!(manager.cost_hint, manager_spec.cost_hint);
    assert_eq!(manager.session_id, None);
    assert_eq!(manager.project_ids, manager_spec.project_ids);

    let binding =
        expect_team_member_binding_notify(&mut fixture.client, "TeamMemberBindingNotify manager")
            .await;
    assert_eq!(binding.member_id, manager.id);
    assert_eq!(binding.current_agent_id, None);
    assert_eq!(binding.status, AgentControlStatus::Idle);

    let mut replay = fixture.connect().await;
    let mut observed = Vec::new();
    while observed.len() < 5 {
        let env = expect_next_event(&mut replay, "team replay").await;
        match env.kind {
            FrameKind::CustomAgentNotify => {
                assert_eq!(
                    env.parse_payload::<CustomAgentNotifyPayload>()
                        .expect("parse replay CustomAgentNotify"),
                    CustomAgentNotifyPayload::Upsert {
                        custom_agent: custom_agent.clone()
                    }
                );
                observed.push(FrameKind::CustomAgentNotify);
            }
            FrameKind::ProjectNotify => {
                match env
                    .parse_payload::<ProjectNotifyPayload>()
                    .expect("parse replay ProjectNotify")
                {
                    ProjectNotifyPayload::Upsert { project: observed } => {
                        assert_eq!(observed.id, project.id)
                    }
                    other => panic!("expected ProjectNotify::Upsert, got {other:?}"),
                }
                observed.push(FrameKind::ProjectNotify);
            }
            FrameKind::TeamNotify => {
                assert_eq!(
                    env.parse_payload::<TeamNotifyPayload>()
                        .expect("parse replay TeamNotify"),
                    TeamNotifyPayload::Upsert { team: team.clone() }
                );
                observed.push(FrameKind::TeamNotify);
            }
            FrameKind::TeamMemberNotify => {
                assert_eq!(
                    env.parse_payload::<TeamMemberNotifyPayload>()
                        .expect("parse replay TeamMemberNotify"),
                    TeamMemberNotifyPayload::Upsert {
                        member: manager.clone()
                    }
                );
                observed.push(FrameKind::TeamMemberNotify);
            }
            FrameKind::TeamMemberBindingNotify => {
                assert_eq!(
                    env.parse_payload::<TeamMemberBindingNotifyPayload>()
                        .expect("parse replay TeamMemberBindingNotify"),
                    TeamMemberBindingNotifyPayload::Upsert {
                        binding: binding.clone()
                    }
                );
                observed.push(FrameKind::TeamMemberBindingNotify);
            }
            other => panic!("unexpected replay event: {other:?}"),
        }
    }

    assert_eq!(
        observed,
        vec![
            FrameKind::ProjectNotify,
            FrameKind::CustomAgentNotify,
            FrameKind::TeamNotify,
            FrameKind::TeamMemberNotify,
            FrameKind::TeamMemberBindingNotify,
        ]
    );
}

#[tokio::test]
async fn team_preset_catalog_replays_before_team_state() {
    let mut fixture = Fixture::new().await;

    let catalog = expect_team_catalog_notify(&mut fixture.client, "initial team catalog").await;

    assert!(
        catalog
            .catalog
            .role_presets
            .iter()
            .any(|preset| preset.name == "Frontend specialist"),
        "catalog should include frontend specialist: {:?}",
        catalog.catalog.role_presets
    );
    assert!(
        catalog
            .catalog
            .personality_presets
            .iter()
            .any(|preset| preset.name == "Skeptical reviewer"),
        "catalog should include personality presets: {:?}",
        catalog.catalog.personality_presets
    );
    assert!(
        catalog
            .catalog
            .team_templates
            .iter()
            .any(|template| template.name == "Small feature team" && template.balanced),
        "catalog should include balanced template: {:?}",
        catalog.catalog.team_templates
    );
}

#[tokio::test]
async fn team_draft_template_shuffle_and_commit_is_atomic() {
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, "draft-commit-project").await;

    fixture
        .client
        .team_draft_create(TeamDraftCreatePayload {
            template_id: Some(TeamTemplateId("small-feature-team".to_owned())),
        })
        .await
        .expect("team_draft_create failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft from template").await;
    assert_eq!(draft.members.len(), 4);
    assert!(
        draft.members.iter().any(|member| member
            .profile
            .as_ref()
            .and_then(|profile| profile.role_preset_id.as_ref())
            == Some(&TeamRolePresetId("frontend-specialist".to_owned()))),
        "template should create profiled frontend member: {draft:?}"
    );
    assert!(
        draft.members.iter().any(
            |member| member.custom_agent_id == Some(CustomAgentId("tyde-team-lead".to_owned()))
        ),
        "template draft should assign built-in team custom agents: {draft:?}"
    );

    fixture
        .client
        .team_draft_update(TeamDraftUpdatePayload::SetName {
            draft_id: draft.id.clone(),
            name: "Generated Feature Team".to_owned(),
        })
        .await
        .expect("team_draft_update name failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft name update").await;
    assert_eq!(draft.name, "Generated Feature Team");

    let manager_id = draft
        .members
        .iter()
        .find(|member| member.org_role == TeamMemberRole::Manager)
        .expect("manager draft member")
        .id
        .clone();
    fixture
        .client
        .team_draft_shuffle(TeamDraftShufflePayload {
            draft_id: draft.id.clone(),
            member_id: Some(manager_id.clone()),
            scope: TeamDraftShuffleScope::Personality,
        })
        .await
        .expect("team_draft_shuffle failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft shuffle").await;
    let shuffled_manager = draft
        .members
        .iter()
        .find(|member| member.id == manager_id)
        .expect("shuffled manager");
    assert!(
        shuffled_manager
            .profile
            .as_ref()
            .and_then(|profile| profile.personality_preset_id.as_ref())
            .is_some(),
        "shuffle should keep server-owned personality profile: {shuffled_manager:?}"
    );
    assert!(
        shuffled_manager.custom_agent_id.is_some(),
        "member shuffle should leave the draft with a concrete custom agent: {shuffled_manager:?}"
    );

    fixture
        .client
        .team_draft_apply_template(TeamDraftApplyTemplatePayload {
            draft_id: draft.id.clone(),
            template_id: TeamTemplateId("debug-squad".to_owned()),
        })
        .await
        .expect("team_draft_apply_template failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft template apply").await;
    assert!(
        draft
            .members
            .iter()
            .any(|member| member.name == "Debug Lead"),
        "debug squad template should replace draft members: {draft:?}"
    );
    assert!(
        draft
            .members
            .iter()
            .any(|member| member.custom_agent_id
                == Some(CustomAgentId("tyde-code-reviewer".to_owned()))),
        "template application should choose built-in custom agents: {draft:?}"
    );

    for member in draft.members.clone() {
        fixture
            .client
            .team_draft_update(TeamDraftUpdatePayload::ReplaceMember {
                draft_id: draft.id.clone(),
                member: complete_draft_member(member, BackendKind::Codex, project.id.clone()),
            })
            .await
            .expect("team_draft_update member failed");
        let _ = expect_team_draft_notify(&mut fixture.client, "draft member completion").await;
    }

    fixture
        .client
        .team_draft_commit(TeamDraftCommitPayload {
            draft_id: draft.id.clone(),
        })
        .await
        .expect("team_draft_commit failed");
    let team = expect_team_notify(&mut fixture.client, "draft commit team").await;
    assert_eq!(team.name, "Generated Feature Team");

    let mut members = Vec::new();
    for _ in 0..draft.members.len() {
        members.push(expect_team_member_notify(&mut fixture.client, "draft commit member").await);
    }
    assert_eq!(members.len(), draft.members.len());
    assert!(
        members
            .iter()
            .all(|member| member.backend_kind == BackendKind::Codex),
        "commit should persist explicit backend on every member: {members:?}"
    );
    assert!(
        members.iter().any(|member| member
            .profile
            .as_ref()
            .and_then(|profile| profile.role_preset_id.as_ref())
            == Some(&TeamRolePresetId("bug-hunter-debugger".to_owned()))),
        "commit should persist structured profile metadata: {members:?}"
    );
    assert!(
        members
            .iter()
            .any(|member| member.custom_agent_id
                == Some(CustomAgentId("tyde-code-reviewer".to_owned()))),
        "commit should persist built-in custom agent selections: {members:?}"
    );
    for member in &members {
        let binding =
            expect_team_member_binding_notify(&mut fixture.client, "draft commit binding").await;
        assert_eq!(binding.member_id, member.id);
    }
    assert_eq!(
        expect_team_draft_delete_notify(&mut fixture.client, "draft commit delete").await,
        draft.id
    );

    let mut replay = fixture.connect().await;
    let mut saw_profile = false;
    while !saw_profile {
        let env = expect_next_event(&mut replay, "profile replay").await;
        match env.kind {
            FrameKind::TeamMemberNotify => {
                let payload = env
                    .parse_payload::<TeamMemberNotifyPayload>()
                    .expect("parse replay TeamMemberNotify");
                if let TeamMemberNotifyPayload::Upsert { member } = payload
                    && member.team_id == team.id
                    && member.profile.is_some()
                {
                    saw_profile = true;
                }
            }
            FrameKind::TeamNotify
            | FrameKind::ProjectNotify
            | FrameKind::TeamMemberBindingNotify => {}
            other => panic!("unexpected replay event while looking for profile: {other:?}"),
        }
    }
}

#[tokio::test]
async fn team_draft_commit_validation_keeps_draft_without_half_created_team() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .team_draft_create(TeamDraftCreatePayload { template_id: None })
        .await
        .expect("team_draft_create failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "blank draft").await;
    fixture
        .client
        .team_draft_update(TeamDraftUpdatePayload::SetName {
            draft_id: draft.id.clone(),
            name: "Invalid Draft".to_owned(),
        })
        .await
        .expect("team_draft_update name failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft name").await;

    fixture
        .client
        .team_draft_commit(TeamDraftCommitPayload {
            draft_id: draft.id.clone(),
        })
        .await
        .expect("team_draft_commit write failed");
    let error = expect_command_error(&mut fixture.client, "draft commit validation").await;
    assert_eq!(error.operation, "team_draft_commit");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("must choose a backend"),
        "unexpected draft validation error: {}",
        error.message
    );

    let mut replay = fixture.connect().await;
    let replayed_draft =
        expect_team_draft_notify(&mut replay, "draft replay after failed commit").await;
    assert_eq!(replayed_draft.id, draft.id);
    assert_eq!(replayed_draft.name, "Invalid Draft");
}

#[tokio::test]
async fn team_draft_personality_update_preserves_edited_fields() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .team_draft_create(TeamDraftCreatePayload {
            template_id: Some(TeamTemplateId("small-feature-team".to_owned())),
        })
        .await
        .expect("team_draft_create failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft from template").await;
    let manager = draft
        .members
        .iter()
        .find(|member| member.org_role == TeamMemberRole::Manager)
        .cloned()
        .expect("manager draft member");
    let manager_id = manager.id.clone();
    let role_preset_id = manager
        .profile
        .as_ref()
        .and_then(|profile| profile.role_preset_id.clone())
        .expect("template manager role preset");
    let edit = protocol::TeamDraftMemberEdit {
        id: manager.id.clone(),
        name: "Edited Lead".to_owned(),
        description: "Edited description".to_owned(),
        custom_agent_id: manager.custom_agent_id.clone(),
        backend_kind: manager.backend_kind,
        cost_hint: manager.cost_hint,
        project_ids: manager.project_ids.clone(),
    };

    fixture
        .client
        .team_draft_update(TeamDraftUpdatePayload::ReplaceMember {
            draft_id: draft.id.clone(),
            member: edit,
        })
        .await
        .expect("team_draft_update member failed");
    let _ = expect_team_draft_notify(&mut fixture.client, "draft member edit").await;

    fixture
        .client
        .team_draft_update(TeamDraftUpdatePayload::SetMemberProfile {
            draft_id: draft.id.clone(),
            member_id: manager_id.clone(),
            role_preset_id: Some(role_preset_id),
            personality_preset_id: Some(TeamPersonalityPresetId("skeptical-reviewer".to_owned())),
            personality_traits: Vec::new(),
        })
        .await
        .expect("team_draft_update profile failed");
    let updated = expect_team_draft_notify(&mut fixture.client, "draft profile update").await;
    let updated_manager = updated
        .members
        .iter()
        .find(|member| member.id == manager_id)
        .expect("updated manager");
    assert_eq!(updated_manager.name, "Edited Lead");
    assert_eq!(updated_manager.description, "Edited description");
    assert_eq!(
        updated_manager
            .profile
            .as_ref()
            .and_then(|profile| profile.personality_preset_id.as_ref()),
        Some(&TeamPersonalityPresetId("skeptical-reviewer".to_owned()))
    );
}

#[tokio::test]
async fn invalid_team_draft_mutation_preserves_draft_for_replay() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .team_draft_create(TeamDraftCreatePayload {
            template_id: Some(TeamTemplateId("small-feature-team".to_owned())),
        })
        .await
        .expect("team_draft_create failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft from template").await;
    let manager_id = draft
        .members
        .iter()
        .find(|member| member.org_role == TeamMemberRole::Manager)
        .expect("manager draft member")
        .id
        .clone();

    fixture
        .client
        .team_draft_update(TeamDraftUpdatePayload::SetMemberProfile {
            draft_id: draft.id.clone(),
            member_id: manager_id,
            role_preset_id: Some(TeamRolePresetId("missing-role-preset".to_owned())),
            personality_preset_id: None,
            personality_traits: Vec::new(),
        })
        .await
        .expect("team_draft_update write failed");
    let error = expect_command_error(&mut fixture.client, "invalid draft profile update").await;
    assert_eq!(error.operation, "team_draft_update");
    assert!(
        error.message.contains("missing role preset"),
        "unexpected draft mutation error: {}",
        error.message
    );

    let mut replay = fixture.connect().await;
    let replayed_draft =
        expect_team_draft_notify(&mut replay, "draft replay after invalid profile mutation").await;
    assert_eq!(replayed_draft, draft);
}

#[tokio::test]
async fn team_member_create_rejects_unknown_custom_agent() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "missing-custom-valid").await;
    let (team, manager) =
        create_team(&mut fixture.client, "Missing Custom", custom_agent.id, None).await;

    fixture
        .client
        .team_member_create(TeamMemberCreatePayload {
            team_id: team.id,
            member: member_spec(
                "report",
                Some(CustomAgentId("does-not-exist".to_owned())),
                manager.project_ids,
            ),
            session_id: None,
        })
        .await
        .expect("team_member_create write failed");

    let error = expect_command_error(&mut fixture.client, "missing custom agent error").await;
    assert_eq!(error.operation, "team_member_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("missing custom agent"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_create_allows_default_agent_with_backend_and_cost() {
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, "default-agent-project").await;
    let mut manager_spec = member_spec_with_profile(
        "manager",
        None,
        BackendKind::Codex,
        Some(SpawnCostHint::Low),
        vec![project.id.clone()],
    );
    manager_spec.profile = Some(TeamMemberPresetProfile {
        role_preset_id: Some(TeamRolePresetId("tech-lead-planner".to_owned())),
        personality_preset_id: Some(TeamPersonalityPresetId("careful-architect".to_owned())),
        personality_traits: vec![
            TeamPersonalityTrait::Cautious,
            TeamPersonalityTrait::TypeSystem,
            TeamPersonalityTrait::Pedagogical,
        ],
    });

    fixture
        .client
        .team_create(TeamCreatePayload {
            name: "Default Agent Team".to_owned(),
            manager: manager_spec.clone(),
        })
        .await
        .expect("team_create failed");

    let team = expect_team_notify(&mut fixture.client, "TeamNotify create").await;
    let manager =
        expect_team_member_notify(&mut fixture.client, "TeamMemberNotify manager create").await;
    let binding =
        expect_team_member_binding_notify(&mut fixture.client, "TeamMemberBinding manager create")
            .await;

    assert_eq!(manager.id, team.manager_member_id);
    assert_eq!(manager.custom_agent_id, None);
    assert_eq!(manager.backend_kind, BackendKind::Codex);
    assert_eq!(manager.cost_hint, Some(SpawnCostHint::Low));
    assert_eq!(manager.project_ids, manager_spec.project_ids);
    assert_eq!(binding.member_id, manager.id);

    fixture
        .client
        .team_member_activate(TeamMemberActivatePayload {
            member_id: manager.id.clone(),
            prompt: Some("Start the team".to_owned()),
            images: None,
        })
        .await
        .expect("team_member_activate failed");
    let new_agent = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "team member NewAgent",
    )
    .await
    .parse_payload::<NewAgentPayload>()
    .expect("parse NewAgentPayload");
    assert_eq!(new_agent.origin, protocol::AgentOrigin::TeamMember);
    assert_eq!(new_agent.backend_kind, BackendKind::Codex);
    assert_eq!(new_agent.custom_agent_id, None);
    assert_eq!(new_agent.team_id.as_ref(), Some(&team.id));
    assert_eq!(new_agent.team_member_id.as_ref(), Some(&manager.id));

    let settings = expect_session_settings_on_stream(
        &mut fixture.client,
        &new_agent.instance_stream,
        "team member SessionSettings",
    )
    .await;
    assert_eq!(
        settings.values.0.get("reasoning_effort"),
        Some(&SessionSettingValue::String("low".to_owned())),
        "team member cost_hint should reach fresh Codex spawn settings: {:?}",
        settings.values
    );
}

#[tokio::test]
async fn team_describe_includes_default_agent_member() {
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, "describe-default-agent-project").await;
    let mut manager_spec = member_spec_with_profile(
        "manager",
        None,
        BackendKind::Codex,
        Some(SpawnCostHint::Low),
        vec![project.id.clone()],
    );
    manager_spec.profile = Some(TeamMemberPresetProfile {
        role_preset_id: Some(TeamRolePresetId("tech-lead-planner".to_owned())),
        personality_preset_id: Some(TeamPersonalityPresetId("careful-architect".to_owned())),
        personality_traits: vec![
            TeamPersonalityTrait::Cautious,
            TeamPersonalityTrait::TypeSystem,
            TeamPersonalityTrait::Pedagogical,
        ],
    });

    fixture
        .client
        .team_create(TeamCreatePayload {
            name: "Describe Default Agent".to_owned(),
            manager: manager_spec,
        })
        .await
        .expect("team_create failed");

    let team = expect_team_notify(&mut fixture.client, "TeamNotify create").await;
    let manager =
        expect_team_member_notify(&mut fixture.client, "TeamMemberNotify manager create").await;
    let _binding =
        expect_team_member_binding_notify(&mut fixture.client, "TeamMemberBinding manager create")
            .await;

    fixture
        .client
        .team_member_activate(TeamMemberActivatePayload {
            member_id: manager.id.clone(),
            prompt: Some("Describe the team".to_owned()),
            images: None,
        })
        .await
        .expect("team_member_activate failed");
    let new_agent = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "team member NewAgent",
    )
    .await
    .parse_payload::<NewAgentPayload>()
    .expect("parse NewAgentPayload");
    let binding = expect_bound_team_member(
        &mut fixture.client,
        &manager.id,
        &new_agent.agent_id,
        "bound team member",
    )
    .await;

    let result = call_agent_control_tool_json(
        &fixture,
        &new_agent.agent_id,
        "tyde_team_describe",
        json!({}),
    )
    .await;
    assert_eq!(result["team"]["id"], json!(team.id));
    let members = result["members"]
        .as_array()
        .expect("members should be an array");
    assert_eq!(
        members.len(),
        1,
        "expected one described member: {result:?}"
    );
    let described = &members[0];
    assert_eq!(described["member"]["id"], json!(manager.id));
    assert!(
        described["member"].get("custom_agent_id").is_none()
            || described["member"]["custom_agent_id"].is_null(),
        "default-agent member should not serialize a custom_agent_id: {described:?}"
    );
    assert!(
        described["custom_agent"].is_null(),
        "default-agent member should have no custom agent summary: {described:?}"
    );
    assert_eq!(described["member"]["backend_kind"], json!("codex"));
    assert_eq!(described["member"]["cost_hint"], json!("low"));
    assert_eq!(
        described["profile"]["role_preset"],
        json!("Tech lead / planner")
    );
    assert_eq!(
        described["profile"]["personality_preset"],
        json!("Careful architect")
    );
    assert!(
        described["profile"]["traits"]
            .as_array()
            .expect("profile traits should be an array")
            .iter()
            .any(|trait_name| trait_name == "Type-system"),
        "profile summary should include readable trait names: {described:?}"
    );
    assert_eq!(described["binding"]["member_id"], json!(binding.member_id));
    assert_eq!(
        described["binding"]["current_agent_id"],
        json!(new_agent.agent_id)
    );
}

#[tokio::test]
async fn team_member_create_rejects_missing_project() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "missing-project-agent").await;
    let (team, _) = create_team(
        &mut fixture.client,
        "Missing Project",
        custom_agent.id.clone(),
        None,
    )
    .await;

    fixture
        .client
        .team_member_create(TeamMemberCreatePayload {
            team_id: team.id,
            member: member_spec(
                "report",
                Some(custom_agent.id),
                vec![protocol::ProjectId("does-not-exist".to_owned())],
            ),
            session_id: None,
        })
        .await
        .expect("team_member_create write failed");

    let error = expect_command_error(&mut fixture.client, "missing project error").await;
    assert_eq!(error.operation, "team_member_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("missing project"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_member_create_rejects_empty_project_ids() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "empty-projects-agent").await;
    let (team, _) = create_team(
        &mut fixture.client,
        "Empty Projects",
        custom_agent.id.clone(),
        None,
    )
    .await;

    fixture
        .client
        .team_member_create(TeamMemberCreatePayload {
            team_id: team.id,
            member: member_spec("report", Some(custom_agent.id), Vec::new()),
            session_id: None,
        })
        .await
        .expect("team_member_create write failed");

    let error = expect_command_error(&mut fixture.client, "empty project_ids error").await;
    assert_eq!(error.operation, "team_member_create");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error.message.contains("project_ids must not be empty"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_create_rejects_missing_inline_manager_payload() {
    let mut fixture = Fixture::new().await;

    send_raw_host_value(
        &mut fixture.client,
        FrameKind::TeamCreate,
        json!({ "name": "No Manager" }),
    )
    .await;

    let error = expect_command_error(&mut fixture.client, "missing manager error").await;
    assert_eq!(error.operation, "team_create");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error.message.contains("missing field `manager`"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_set_manager_rejects_current_manager() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "current-manager-agent").await;
    let (team, manager) = create_team(
        &mut fixture.client,
        "Current Manager",
        custom_agent.id,
        None,
    )
    .await;

    fixture
        .client
        .team_set_manager(TeamSetManagerPayload {
            team_id: team.id,
            new_manager_member_id: manager.id,
        })
        .await
        .expect("team_set_manager write failed");

    let error = expect_command_error(&mut fixture.client, "current manager error").await;
    assert_eq!(error.operation, "team_set_manager");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("already the manager"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_set_manager_rejects_missing_target() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "missing-target-agent").await;
    let (team, _) = create_team(&mut fixture.client, "Missing Target", custom_agent.id, None).await;

    fixture
        .client
        .team_set_manager(TeamSetManagerPayload {
            team_id: team.id,
            new_manager_member_id: TeamMemberId("missing-member".to_owned()),
        })
        .await
        .expect("team_set_manager write failed");

    let error = expect_command_error(&mut fixture.client, "missing target error").await;
    assert_eq!(error.operation, "team_set_manager");
    assert_eq!(error.code, CommandErrorCode::NotFound);
    assert!(!error.fatal);
    assert!(
        error.message.contains("missing member"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_set_manager_rejects_member_from_different_team() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "different-team-agent").await;
    let (team_a, _) = create_team(
        &mut fixture.client,
        "Different Team A",
        custom_agent.id.clone(),
        None,
    )
    .await;
    let (team_b, _) = create_team(
        &mut fixture.client,
        "Different Team B",
        custom_agent.id.clone(),
        None,
    )
    .await;
    let team_b_report = create_report(
        &mut fixture.client,
        team_b.id,
        "report",
        custom_agent.id,
        None,
    )
    .await;

    fixture
        .client
        .team_set_manager(TeamSetManagerPayload {
            team_id: team_a.id,
            new_manager_member_id: team_b_report.id,
        })
        .await
        .expect("team_set_manager write failed");

    let error = expect_command_error(&mut fixture.client, "different team error").await;
    assert_eq!(error.operation, "team_set_manager");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error.message.contains("does not belong to team"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_member_delete_rejects_active_manager() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "delete-manager-agent").await;
    let (_, manager) =
        create_team(&mut fixture.client, "Delete Manager", custom_agent.id, None).await;

    fixture
        .client
        .team_member_delete(TeamMemberDeletePayload { id: manager.id })
        .await
        .expect("team_member_delete write failed");

    let error = expect_command_error(&mut fixture.client, "delete manager error").await;
    assert_eq!(error.operation, "team_member_delete");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("active manager"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn team_delete_hard_removes_team_and_members() {
    let mut fixture = Fixture::new().await;
    let (_, _, team, manager, report) = create_team_with_report(&mut fixture, "hard-delete").await;

    fixture
        .client
        .team_delete(TeamDeletePayload {
            id: team.id.clone(),
        })
        .await
        .expect("team_delete failed");
    let deleted_team = expect_team_delete_notify(&mut fixture.client, "TeamNotify delete").await;
    assert_eq!(deleted_team.id, team.id);

    let deleted_manager =
        expect_team_member_delete_notify(&mut fixture.client, "TeamMemberNotify manager delete")
            .await;
    let deleted_report =
        expect_team_member_delete_notify(&mut fixture.client, "TeamMemberNotify report delete")
            .await;
    assert_eq!(deleted_manager.id, manager.id);
    assert_eq!(deleted_report.id, report.id);

    let manager_binding = expect_team_member_binding_delete_notify(
        &mut fixture.client,
        "TeamMemberBindingNotify manager delete",
    )
    .await;
    let report_binding = expect_team_member_binding_delete_notify(
        &mut fixture.client,
        "TeamMemberBindingNotify report delete",
    )
    .await;
    assert_eq!(manager_binding.member_id, manager.id);
    assert_eq!(report_binding.member_id, report.id);

    fixture
        .client
        .team_rename(TeamRenamePayload {
            id: team.id,
            name: "Should Fail".to_owned(),
        })
        .await
        .expect("team_rename write failed");

    let error = expect_command_error(&mut fixture.client, "deleted rename error").await;
    assert_eq!(error.operation, "team_rename");
    assert_eq!(error.code, CommandErrorCode::NotFound);
    assert!(!error.fatal);
    assert!(
        error.message.contains("missing team"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn custom_agent_delete_rejects_active_team_member_reference() {
    let mut fixture = Fixture::new().await;
    let (custom_agent, _, _, _, _) = create_team_with_report(&mut fixture, "custom-delete").await;

    fixture
        .client
        .custom_agent_delete(CustomAgentDeletePayload {
            id: custom_agent.id,
        })
        .await
        .expect("custom_agent_delete write failed");

    let error = expect_command_error(&mut fixture.client, "referenced custom agent delete").await;
    assert_eq!(error.operation, "custom_agent_delete");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("team member"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn custom_agent_delete_succeeds_after_team_member_delete() {
    let mut fixture = Fixture::new().await;
    let (_manager_agent, report_agent, report) =
        create_team_with_distinct_report_agent(&mut fixture, "custom-delete-after-delete").await;

    fixture
        .client
        .team_member_delete(TeamMemberDeletePayload {
            id: report.id.clone(),
        })
        .await
        .expect("team_member_delete failed");
    let deleted_report =
        expect_team_member_delete_notify(&mut fixture.client, "TeamMemberNotify deleted report")
            .await;
    assert_eq!(deleted_report.id, report.id);
    let binding = expect_team_member_binding_delete_notify(
        &mut fixture.client,
        "TeamMemberBindingNotify deleted report",
    )
    .await;
    assert_eq!(binding.member_id, report.id);

    fixture
        .client
        .custom_agent_delete(CustomAgentDeletePayload {
            id: report_agent.id.clone(),
        })
        .await
        .expect("custom_agent_delete failed");
    assert_eq!(
        expect_custom_agent_notify(&mut fixture.client, "CustomAgentNotify delete").await,
        CustomAgentNotifyPayload::Delete {
            id: report_agent.id
        }
    );
    let _fresh = fixture.connect_fresh_host().await;
}

#[tokio::test]
async fn project_delete_rejects_active_team_member_reference() {
    let mut fixture = Fixture::new().await;
    let (_, project, _, _, _) = create_team_with_report(&mut fixture, "project-delete").await;

    fixture
        .client
        .project_delete(ProjectDeletePayload { id: project.id })
        .await
        .expect("project_delete write failed");

    let error = expect_command_error(&mut fixture.client, "referenced project delete").await;
    assert_eq!(error.operation, "project_delete");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("team member"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn project_delete_succeeds_after_team_member_delete() {
    let mut fixture = Fixture::new().await;
    let custom_agent =
        upsert_custom_agent(&mut fixture.client, "project-delete-after-member-delete").await;
    let manager_project = create_project(&mut fixture.client, "manager-delete-project").await;
    let report_project = create_project(&mut fixture.client, "report-delete-project").await;
    let (team, _) = create_team(
        &mut fixture.client,
        "Project Delete After Member Delete",
        custom_agent.id.clone(),
        Some(manager_project.id),
    )
    .await;
    let report = create_report(
        &mut fixture.client,
        team.id,
        "report",
        custom_agent.id,
        Some(report_project.id.clone()),
    )
    .await;

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: report_project.id.clone(),
        })
        .await
        .expect("project_delete write failed");
    let error = expect_command_error(&mut fixture.client, "referenced report project delete").await;
    assert_eq!(error.operation, "project_delete");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(
        error.message.contains("team member"),
        "unexpected error: {}",
        error.message
    );

    fixture
        .client
        .team_member_delete(TeamMemberDeletePayload { id: report.id })
        .await
        .expect("team_member_delete failed");
    let _ =
        expect_team_member_delete_notify(&mut fixture.client, "TeamMemberNotify deleted report")
            .await;
    let _ = expect_team_member_binding_delete_notify(
        &mut fixture.client,
        "TeamMemberBindingNotify deleted report",
    )
    .await;
    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: report_project.id.clone(),
        })
        .await
        .expect("project_delete failed");
    let env = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "ProjectNotify delete",
    )
    .await;
    match env
        .parse_payload::<ProjectNotifyPayload>()
        .expect("parse ProjectNotify delete")
    {
        ProjectNotifyPayload::Delete { project: deleted } => {
            assert_eq!(deleted.id, report_project.id)
        }
        other => panic!("expected ProjectNotify::Delete, got {other:?}"),
    }
    let _fresh = fixture.connect_fresh_host().await;
}

#[tokio::test]
async fn manager_replacement_swaps_roles_atomically() {
    let mut fixture = Fixture::new().await;
    let (_, _, team, manager, report) = create_team_with_report(&mut fixture, "manager-swap").await;

    fixture
        .client
        .team_set_manager(TeamSetManagerPayload {
            team_id: team.id.clone(),
            new_manager_member_id: report.id.clone(),
        })
        .await
        .expect("team_set_manager failed");

    let updated_team = expect_team_notify(&mut fixture.client, "TeamNotify manager swap").await;
    assert_eq!(updated_team.id, team.id);
    assert_eq!(updated_team.manager_member_id, report.id);

    let demoted = expect_team_member_notify(&mut fixture.client, "demoted manager").await;
    let promoted = expect_team_member_notify(&mut fixture.client, "promoted report").await;
    assert_eq!(demoted.id, manager.id);
    assert_eq!(demoted.role, TeamMemberRole::Report);
    assert_eq!(promoted.id, report.id);
    assert_eq!(promoted.role, TeamMemberRole::Manager);

    fixture
        .client
        .team_member_delete(TeamMemberDeletePayload {
            id: manager.id.clone(),
        })
        .await
        .expect("delete demoted manager failed");
    let deleted =
        expect_team_member_delete_notify(&mut fixture.client, "delete demoted manager").await;
    assert_eq!(deleted.id, manager.id);
    assert_eq!(deleted.role, TeamMemberRole::Report);
    let binding =
        expect_team_member_binding_delete_notify(&mut fixture.client, "demoted manager binding")
            .await;
    assert_eq!(binding.member_id, deleted.id);
}

#[tokio::test]
async fn replay_order_pins_dependencies_before_teams() {
    let mut fixture = Fixture::new().await;
    let (_custom_agent, _project, _team, _manager, _report) =
        create_team_with_report(&mut fixture, "replay-order").await;

    let mut replay = fixture.connect().await;
    let mut observed = Vec::new();
    while observed.len() < 5 {
        let env = expect_next_event(&mut replay, "team dependency replay").await;
        match env.kind {
            FrameKind::ProjectNotify
            | FrameKind::CustomAgentNotify
            | FrameKind::TeamNotify
            | FrameKind::TeamMemberNotify
            | FrameKind::TeamMemberBindingNotify => {
                observed.push(env.kind);
            }
            other => panic!("unexpected replay frame: {other:?}"),
        }
    }

    assert_eq!(
        observed,
        vec![
            FrameKind::ProjectNotify,
            FrameKind::CustomAgentNotify,
            FrameKind::TeamNotify,
            FrameKind::TeamMemberNotify,
            FrameKind::TeamMemberNotify,
        ]
    );
}

async fn expect_team_member_shuffle_suggestion(
    client: &mut client::Connection,
    context: &str,
) -> protocol::TeamMemberShuffleSuggestionNotifyPayload {
    let env = expect_kind(
        client,
        FrameKind::TeamMemberShuffleSuggestionNotify,
        context,
    )
    .await;
    env.parse_payload::<protocol::TeamMemberShuffleSuggestionNotifyPayload>()
        .expect("parse TeamMemberShuffleSuggestionNotify")
}

#[tokio::test]
async fn team_member_shuffle_emits_server_owned_suggestion() {
    let mut fixture = Fixture::new().await;
    let custom_agent = upsert_custom_agent(&mut fixture.client, "shuffle-team-agent").await;
    let (team, _manager) =
        create_team(&mut fixture.client, "Shuffle Team", custom_agent.id, None).await;

    fixture
        .client
        .team_member_shuffle(protocol::TeamMemberShufflePayload {
            team_id: team.id.clone(),
        })
        .await
        .expect("team_member_shuffle failed");
    let notify = expect_team_member_shuffle_suggestion(&mut fixture.client, "first shuffle").await;
    assert_eq!(notify.team_id, team.id);
    assert!(
        !notify.suggestion.name.trim().is_empty(),
        "suggestion name must be non-empty"
    );
    assert!(
        !notify.suggestion.description.trim().is_empty(),
        "suggestion description must be non-empty"
    );
    assert!(
        notify.suggestion.profile.role_preset_id.is_some(),
        "suggestion must carry a role preset id from the server catalog"
    );
    assert!(
        notify.suggestion.profile.personality_preset_id.is_some(),
        "suggestion must carry a personality preset id from the server catalog"
    );
    assert!(
        notify.suggestion.custom_agent_id.is_some(),
        "suggestion must carry a default custom agent from the server catalog"
    );

    // Shuffling against a team that does not exist on the host is an
    // error, not a silent no-op.
    fixture
        .client
        .team_member_shuffle(protocol::TeamMemberShufflePayload {
            team_id: TeamId("does-not-exist".to_owned()),
        })
        .await
        .expect("team_member_shuffle send failed");
    let err = expect_command_error(&mut fixture.client, "shuffle missing team error").await;
    assert_eq!(err.operation, "team_member_shuffle");
}

#[tokio::test]
async fn team_draft_replace_member_preserves_server_owned_profile() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .team_draft_create(TeamDraftCreatePayload {
            template_id: Some(TeamTemplateId("small-feature-team".to_owned())),
        })
        .await
        .expect("team_draft_create failed");
    let draft = expect_team_draft_notify(&mut fixture.client, "draft from template").await;
    let report = draft
        .members
        .iter()
        .find(|member| member.org_role == TeamMemberRole::Report)
        .cloned()
        .expect("template report member");
    let original_profile = report.profile.clone().expect("template report has profile");
    let original_role = report.org_role;

    // The narrowed ReplaceMember payload cannot carry org_role/profile.
    // Sending an edit that updates only the user-editable fields must
    // leave the server-owned fields untouched.
    let edit = protocol::TeamDraftMemberEdit {
        id: report.id.clone(),
        name: "Renamed Report".to_owned(),
        description: "Renamed description".to_owned(),
        custom_agent_id: report.custom_agent_id.clone(),
        backend_kind: report.backend_kind,
        cost_hint: report.cost_hint,
        project_ids: report.project_ids.clone(),
    };
    fixture
        .client
        .team_draft_update(TeamDraftUpdatePayload::ReplaceMember {
            draft_id: draft.id.clone(),
            member: edit,
        })
        .await
        .expect("team_draft_update replace member failed");
    let updated_draft =
        expect_team_draft_notify(&mut fixture.client, "draft after replace_member").await;
    let updated_report = updated_draft
        .members
        .iter()
        .find(|member| member.id == report.id)
        .expect("updated report");
    assert_eq!(updated_report.name, "Renamed Report");
    assert_eq!(updated_report.description, "Renamed description");
    assert_eq!(
        updated_report.org_role, original_role,
        "ReplaceMember must not flip org_role on the server"
    );
    assert_eq!(
        updated_report.profile.as_ref(),
        Some(&original_profile),
        "ReplaceMember must not let the client overwrite the server-owned profile"
    );
}

#[tokio::test]
async fn custom_agent_delete_rejected_for_builtin_role_preset_default() {
    let mut fixture = Fixture::new().await;
    // Wait for the host to replay built-in team CustomAgents and the
    // preset catalog so we know seeding has run.
    let _catalog = expect_team_catalog_notify(&mut fixture.client, "preset catalog").await;

    fixture
        .client
        .custom_agent_delete(CustomAgentDeletePayload {
            id: CustomAgentId("tyde-frontend-engineer".to_owned()),
        })
        .await
        .expect("custom_agent_delete send failed");
    let err = expect_command_error(
        &mut fixture.client,
        "delete built-in custom agent should error",
    )
    .await;
    assert_eq!(err.operation, "custom_agent_delete");
    assert_eq!(err.code, CommandErrorCode::Conflict);
    assert!(
        err.message.contains("role preset"),
        "error should explain the role preset link: {}",
        err.message
    );
}
