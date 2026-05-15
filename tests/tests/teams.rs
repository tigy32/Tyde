mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentControlStatus, CommandErrorCode, CommandErrorPayload, CustomAgent,
    CustomAgentDeletePayload, CustomAgentId, CustomAgentNotifyPayload, CustomAgentUpsertPayload,
    Envelope, FrameKind, Project, ProjectCreatePayload, ProjectDeletePayload, ProjectNotifyPayload,
    Team, TeamCreatePayload, TeamDeletePayload, TeamId, TeamMember, TeamMemberBindingNotifyPayload,
    TeamMemberBindingPayload, TeamMemberCreatePayload, TeamMemberCreateSpec,
    TeamMemberDeletePayload, TeamMemberId, TeamMemberNotifyPayload, TeamMemberRole,
    TeamMemberState, TeamNotifyPayload, TeamRenamePayload, TeamSetManagerPayload, ToolPolicy,
    write_envelope,
};
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
    custom_agent_id: CustomAgentId,
    project_ids: Vec<protocol::ProjectId>,
) -> TeamMemberCreateSpec {
    TeamMemberCreateSpec {
        name: name.to_owned(),
        description: format!("{name} description"),
        custom_agent_id,
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
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots: vec![format!("/tmp/tyde-team-project-{name}")],
        })
        .await
        .expect("project_create failed");
    expect_project_notify(client, "ProjectNotify upsert").await
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
            manager: member_spec("manager", custom_agent_id, project_ids),
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
            member: member_spec(name, custom_agent_id, project_ids),
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
    let manager_spec = member_spec("manager", custom_agent.id.clone(), vec![project.id.clone()]);

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
    assert_eq!(manager.custom_agent_id, custom_agent.id);
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
async fn team_member_create_rejects_missing_custom_agent() {
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
                CustomAgentId("does-not-exist".to_owned()),
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
                custom_agent.id,
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
            member: member_spec("report", custom_agent.id, Vec::new()),
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
