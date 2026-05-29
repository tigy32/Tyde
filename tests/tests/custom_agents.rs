mod fixture;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentBootstrapPayload, AgentErrorPayload, AgentStartPayload, BackendKind,
    CommandErrorCode, CommandErrorPayload, CustomAgent, CustomAgentDeletePayload, CustomAgentId,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, Envelope, FrameKind, McpServerConfig,
    McpServerDeletePayload, McpServerId, McpServerNotifyPayload, McpServerUpsertPayload,
    McpTransportConfig, NewAgentPayload, ProjectCreatePayload, ProjectNotifyPayload, Skill,
    SkillId, SkillNotifyPayload, SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload,
    Steering, SteeringDeletePayload, SteeringId, SteeringNotifyPayload, SteeringScope,
    SteeringUpsertPayload, ToolPolicy,
};
use serde_json::to_string_pretty;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

fn pending_agent_events() -> &'static Mutex<HashMap<protocol::StreamPath, VecDeque<Envelope>>> {
    static PENDING: OnceLock<Mutex<HashMap<protocol::StreamPath, VecDeque<Envelope>>>> =
        OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pop_pending_agent_event(stream: &protocol::StreamPath, kind: FrameKind) -> Option<Envelope> {
    let mut pending = pending_agent_events()
        .lock()
        .expect("pending agent event mutex poisoned");
    let queue = pending.get_mut(stream)?;
    let index = queue.iter().position(|env| env.kind == kind)?;
    let env = queue.remove(index);
    if queue.is_empty() {
        pending.remove(stream);
    }
    env
}

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind == FrameKind::AgentBootstrap {
            let payload: AgentBootstrapPayload =
                env.parse_payload().expect("parse AgentBootstrapPayload");
            let mut events = payload
                .events
                .into_iter()
                .filter_map(|event| bootstrap_event_envelope(&env.stream, env.seq, event));
            if let Some(first) = events.next() {
                let mut rest = events.collect::<VecDeque<_>>();
                if !rest.is_empty() {
                    pending_agent_events()
                        .lock()
                        .expect("pending agent event mutex poisoned")
                        .entry(env.stream.clone())
                        .or_default()
                        .append(&mut rest);
                }
                return first;
            }
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionList
                | FrameKind::ProjectBootstrap
                | FrameKind::ProjectGitStatus
                | FrameKind::ProjectFileList
        ) {
            continue;
        }
        return env;
    }
}

fn bootstrap_event_envelope(
    stream: &protocol::StreamPath,
    seq: u64,
    event: AgentBootstrapEvent,
) -> Option<Envelope> {
    match event {
        AgentBootstrapEvent::AgentStart(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::AgentStart,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::AgentError(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::AgentError,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::ChatEvent(payload) => Some(Envelope::from_payload(
            stream.clone(),
            FrameKind::ChatEvent,
            seq,
            &payload,
        )),
        AgentBootstrapEvent::SessionSettings(_) | AgentBootstrapEvent::QueuedMessages(_) => None,
    }
    .map(|result| result.expect("serialize AgentBootstrap event"))
}

async fn raw_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

fn builtin_team_custom_agent_ids() -> HashSet<&'static str> {
    [
        "tyde-team-lead",
        "tyde-code-reviewer",
        "tyde-frontend-engineer",
        "tyde-backend-engineer",
        "tyde-test-qa-engineer",
        "tyde-debugger",
    ]
    .into_iter()
    .collect()
}

fn collect_builtin_team_custom_agents_from_bootstrap(
    bootstrap: &protocol::HostBootstrapPayload,
) -> HashMap<CustomAgentId, CustomAgent> {
    let expected = builtin_team_custom_agent_ids();
    bootstrap
        .custom_agents
        .iter()
        .filter(|agent| expected.contains(agent.id.0.as_str()))
        .map(|agent| (agent.id.clone(), agent.clone()))
        .collect()
}

async fn expect_custom_agent_upsert_raw(
    client: &mut client::Connection,
    id: &CustomAgentId,
    context: &str,
) -> CustomAgent {
    loop {
        let env = raw_next_event(client, context).await;
        if env.kind != FrameKind::CustomAgentNotify {
            continue;
        }
        match env
            .parse_payload::<CustomAgentNotifyPayload>()
            .expect("parse CustomAgentNotifyPayload")
        {
            CustomAgentNotifyPayload::Upsert { custom_agent } if &custom_agent.id == id => {
                return custom_agent;
            }
            CustomAgentNotifyPayload::Upsert { .. } | CustomAgentNotifyPayload::Delete { .. } => {}
        }
    }
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::CommandError);
    env.parse_payload()
        .expect("failed to parse CommandErrorPayload")
}

async fn expect_agent_error_containing(
    client: &mut client::Connection,
    stream: &protocol::StreamPath,
    expected: &str,
    context: &str,
) -> AgentErrorPayload {
    loop {
        if let Some(env) = pop_pending_agent_event(stream, FrameKind::AgentError) {
            let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
            if payload.message.contains(expected) {
                return payload;
            }
        }
        let env = expect_next_event(client, context).await;
        if env.kind != FrameKind::AgentError || env.stream != *stream {
            continue;
        }
        let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
        if payload.message.contains(expected) {
            return payload;
        }
    }
}

async fn expect_session_list(
    client: &mut client::Connection,
    context: &str,
) -> protocol::SessionListPayload {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
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
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::ProjectBootstrap
                | FrameKind::ProjectGitStatus
                | FrameKind::ProjectFileList
        ) {
            continue;
        }
        assert_eq!(env.kind, FrameKind::SessionList);
        return env
            .parse_payload()
            .expect("failed to parse SessionListPayload");
    }
}

async fn expect_turn_text(client: &mut client::Connection, context: &str) -> String {
    let env = expect_next_event(client, &format!("{context}: TypingStatusChanged(true)")).await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let env = expect_next_event(client, &format!("{context}: StreamStart")).await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let env = expect_next_event(client, &format!("{context}: StreamDelta")).await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let delta: protocol::ChatEvent = env.parse_payload().expect("parse StreamDelta ChatEvent");
    let text = match delta {
        protocol::ChatEvent::StreamDelta(data) => data.text,
        other => panic!("expected StreamDelta, got {other:?}"),
    };
    let env = expect_next_event(client, &format!("{context}: StreamEnd")).await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    let env = expect_next_event(client, &format!("{context}: TypingStatusChanged(false)")).await;
    assert_eq!(env.kind, FrameKind::ChatEvent);
    text
}

async fn wait_for_session_list(
    client: &mut client::Connection,
    context: &str,
) -> protocol::SessionListPayload {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
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
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::ProjectBootstrap
                | FrameKind::ProjectGitStatus
                | FrameKind::ProjectFileList
        ) {
            continue;
        }
        if env.kind == FrameKind::SessionList {
            return env.parse_payload().expect("parse SessionListPayload");
        }
    }
}

fn write_skill(store_dir: &Path, skill: &Skill, body: &str) {
    let skill_dir = store_dir.join("skills").join(&skill.name);
    fs::create_dir_all(&skill_dir)
        .unwrap_or_else(|err| panic!("create skill dir {} failed: {err}", skill_dir.display()));
    fs::write(
        skill_dir.join("metadata.json"),
        to_string_pretty(skill).expect("serialize skill metadata"),
    )
    .unwrap_or_else(|err| panic!("write skill metadata failed: {err}"));
    fs::write(skill_dir.join("SKILL.md"), body)
        .unwrap_or_else(|err| panic!("write skill body failed: {err}"));
}

fn remove_skill(store_dir: &Path, skill_name: &str) {
    let skill_dir = store_dir.join("skills").join(skill_name);
    if skill_dir.exists() {
        fs::remove_dir_all(&skill_dir)
            .unwrap_or_else(|err| panic!("remove skill dir {} failed: {err}", skill_dir.display()));
    }
}

fn ensure_dir(path: &str) {
    fs::create_dir_all(path).unwrap_or_else(|err| panic!("create dir {path} failed: {err}"));
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> protocol::Project {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectNotify);
    match env
        .parse_payload::<ProjectNotifyPayload>()
        .expect("parse ProjectNotifyPayload")
    {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected ProjectNotify::Upsert, got {other:?}"),
    }
}

fn sample_mcp_server(id: &str, name: &str) -> McpServerConfig {
    McpServerConfig {
        id: McpServerId(id.to_string()),
        name: name.to_string(),
        transport: McpTransportConfig::Stdio {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            env: HashMap::new(),
        },
    }
}

fn sample_skill(id: &str, name: &str) -> Skill {
    Skill {
        id: SkillId(id.to_string()),
        name: name.to_string(),
        title: Some(format!("{name} title")),
        description: Some(format!("{name} description")),
    }
}

fn sample_custom_agent(
    id: &str,
    skill_ids: Vec<SkillId>,
    mcp_server_ids: Vec<McpServerId>,
    tool_policy: ToolPolicy,
) -> CustomAgent {
    CustomAgent {
        id: CustomAgentId(id.to_string()),
        name: format!("{id} name"),
        description: format!("{id} description"),
        instructions: Some(format!("{id} instructions")),
        skill_ids,
        mcp_server_ids,
        tool_policy,
    }
}

#[tokio::test]
async fn builtin_team_custom_agents_seed_and_preserve_user_edits() {
    let mut fixture = Fixture::new().await;
    let builtins = collect_builtin_team_custom_agents_from_bootstrap(&fixture.bootstrap);
    assert_eq!(
        builtins.len(),
        6,
        "expected six built-in team custom agents: {builtins:?}"
    );
    let reviewer_id = CustomAgentId("tyde-code-reviewer".to_owned());
    let reviewer = builtins
        .get(&reviewer_id)
        .expect("built-in Code Reviewer should be seeded");
    assert_eq!(reviewer.name, "Code Reviewer");
    assert!(
        reviewer
            .instructions
            .as_deref()
            .is_some_and(|instructions| instructions.contains("Review workflow")),
        "Code Reviewer should start with review defaults: {reviewer:?}"
    );
    let debugger = builtins
        .get(&CustomAgentId("tyde-debugger".to_owned()))
        .expect("built-in Debugger should be seeded");
    assert_eq!(debugger.name, "Debugger");
    assert!(
        debugger
            .instructions
            .as_deref()
            .is_some_and(|instructions| instructions.contains("Form theories")),
        "Debugger should start with debugging defaults: {debugger:?}"
    );

    let mut edited = reviewer.clone();
    edited.description = "Custom review policy".to_owned();
    edited.instructions = Some("Always check migration tests before approval.".to_owned());
    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: edited.clone(),
        })
        .await
        .expect("custom_agent_upsert built-in override failed");
    let notified =
        expect_custom_agent_upsert_raw(&mut fixture.client, &reviewer_id, "edited built-in upsert")
            .await;
    assert_eq!(notified, edited);

    let (_fresh, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let replayed = collect_builtin_team_custom_agents_from_bootstrap(&bootstrap);
    assert_eq!(
        replayed.get(&reviewer_id),
        Some(&edited),
        "built-in seeding must not overwrite user edits"
    );
}

#[tokio::test]
async fn host_customization_upsert_delete_round_trip_and_notify() {
    let mut fixture = Fixture::new().await;
    let mcp_server = sample_mcp_server("docs", "docs-server");
    let skill = sample_skill("lint", "lint");
    let steering = Steering {
        id: SteeringId("host-steering".to_string()),
        scope: SteeringScope::Host,
        title: "Host Rules".to_string(),
        content: "Prefer deterministic tools.".to_string(),
    };
    let custom_agent = sample_custom_agent(
        "reviewer",
        vec![skill.id.clone()],
        vec![mcp_server.id.clone()],
        ToolPolicy::Unrestricted,
    );

    fixture
        .client
        .mcp_server_upsert(McpServerUpsertPayload {
            mcp_server: mcp_server.clone(),
        })
        .await
        .expect("mcp_server_upsert failed");
    let env = expect_next_event(&mut fixture.client, "McpServerNotify upsert").await;
    assert_eq!(env.kind, FrameKind::McpServerNotify);
    assert_eq!(
        env.parse_payload::<McpServerNotifyPayload>()
            .expect("parse McpServerNotifyPayload"),
        McpServerNotifyPayload::Upsert {
            mcp_server: mcp_server.clone()
        }
    );

    write_skill(
        fixture.store_dir(),
        &skill,
        "Use cargo fmt before final output.",
    );
    fixture
        .client
        .skill_refresh(SkillRefreshPayload::default())
        .await
        .expect("skill_refresh failed");
    let env = expect_next_event(&mut fixture.client, "SkillNotify upsert").await;
    assert_eq!(env.kind, FrameKind::SkillNotify);
    assert_eq!(
        env.parse_payload::<SkillNotifyPayload>()
            .expect("parse SkillNotifyPayload"),
        SkillNotifyPayload::Upsert {
            skill: skill.clone()
        }
    );

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: steering.clone(),
        })
        .await
        .expect("steering_upsert failed");
    let env = expect_next_event(&mut fixture.client, "SteeringNotify upsert").await;
    assert_eq!(env.kind, FrameKind::SteeringNotify);
    assert_eq!(
        env.parse_payload::<SteeringNotifyPayload>()
            .expect("parse SteeringNotifyPayload"),
        SteeringNotifyPayload::Upsert {
            steering: steering.clone()
        }
    );

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    let env = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;
    assert_eq!(env.kind, FrameKind::CustomAgentNotify);
    assert_eq!(
        env.parse_payload::<CustomAgentNotifyPayload>()
            .expect("parse CustomAgentNotifyPayload"),
        CustomAgentNotifyPayload::Upsert {
            custom_agent: custom_agent.clone()
        }
    );

    let (_replay, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(bootstrap.mcp_servers.contains(&mcp_server));
    assert!(bootstrap.skills.contains(&skill));
    assert!(bootstrap.steering.contains(&steering));
    assert!(bootstrap.custom_agents.contains(&custom_agent));

    fixture
        .client
        .custom_agent_delete(CustomAgentDeletePayload {
            id: custom_agent.id.clone(),
        })
        .await
        .expect("custom_agent_delete failed");
    let env = expect_next_event(&mut fixture.client, "CustomAgentNotify delete").await;
    assert_eq!(
        env.parse_payload::<CustomAgentNotifyPayload>()
            .expect("parse CustomAgentNotify delete"),
        CustomAgentNotifyPayload::Delete {
            id: custom_agent.id.clone()
        }
    );

    fixture
        .client
        .steering_delete(SteeringDeletePayload {
            id: steering.id.clone(),
        })
        .await
        .expect("steering_delete failed");
    let env = expect_next_event(&mut fixture.client, "SteeringNotify delete").await;
    assert_eq!(
        env.parse_payload::<SteeringNotifyPayload>()
            .expect("parse SteeringNotify delete"),
        SteeringNotifyPayload::Delete {
            id: steering.id.clone()
        }
    );

    fixture
        .client
        .mcp_server_delete(McpServerDeletePayload {
            id: mcp_server.id.clone(),
        })
        .await
        .expect("mcp_server_delete failed");
    let env = expect_next_event(&mut fixture.client, "McpServerNotify delete").await;
    assert_eq!(
        env.parse_payload::<McpServerNotifyPayload>()
            .expect("parse McpServerNotify delete"),
        McpServerNotifyPayload::Delete {
            id: mcp_server.id.clone()
        }
    );

    remove_skill(fixture.store_dir(), &skill.name);
    fixture
        .client
        .skill_refresh(SkillRefreshPayload::default())
        .await
        .expect("skill_refresh delete failed");
    let env = expect_next_event(&mut fixture.client, "SkillNotify delete").await;
    assert_eq!(
        env.parse_payload::<SkillNotifyPayload>()
            .expect("parse SkillNotify delete"),
        SkillNotifyPayload::Delete {
            id: skill.id.clone()
        }
    );
}

#[tokio::test]
async fn invalid_custom_agent_upsert_with_blank_description_keeps_connection_alive() {
    let mut fixture = Fixture::new().await;
    let custom_agent = CustomAgent {
        id: CustomAgentId("invalid-agent".to_string()),
        name: "Invalid Agent".to_string(),
        description: "   ".to_string(),
        instructions: Some("still has instructions".to_string()),
        skill_ids: Vec::new(),
        mcp_server_ids: Vec::new(),
        tool_policy: ToolPolicy::Unrestricted,
    };

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload { custom_agent })
        .await
        .expect("custom_agent_upsert write failed");

    fixture
        .client
        .list_sessions(protocol::ListSessionsPayload::default())
        .await
        .expect("list_sessions after invalid custom agent upsert failed");

    let error = expect_command_error(&mut fixture.client, "command error").await;
    assert_eq!(error.operation, "custom_agent_upsert");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error.message.contains("description must not be empty"),
        "unexpected custom agent error: {}",
        error.message
    );

    let list = expect_session_list(&mut fixture.client, "SessionList").await;
    assert!(list.sessions.is_empty());
}

#[tokio::test]
async fn deleting_referenced_mcp_server_keeps_connection_alive() {
    let mut fixture = Fixture::new().await;
    let mcp_server = sample_mcp_server("docs", "docs-server");
    let custom_agent = sample_custom_agent(
        "reviewer",
        Vec::new(),
        vec![mcp_server.id.clone()],
        ToolPolicy::Unrestricted,
    );

    fixture
        .client
        .mcp_server_upsert(McpServerUpsertPayload {
            mcp_server: mcp_server.clone(),
        })
        .await
        .expect("mcp_server_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "McpServerNotify upsert").await;

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;

    fixture
        .client
        .mcp_server_delete(McpServerDeletePayload {
            id: mcp_server.id.clone(),
        })
        .await
        .expect("mcp_server_delete failed");

    fixture
        .client
        .list_sessions(protocol::ListSessionsPayload::default())
        .await
        .expect("list_sessions after referenced MCP delete failed");

    let error = expect_command_error(&mut fixture.client, "command error").await;
    assert_eq!(error.operation, "mcp_server_delete");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("referenced by custom agent"),
        "unexpected MCP delete error: {}",
        error.message
    );

    let list = expect_session_list(&mut fixture.client, "SessionList").await;
    assert!(list.sessions.is_empty());
}

#[tokio::test]
async fn replay_order_replays_customization_before_agents() {
    let mut fixture = Fixture::new().await;
    ensure_dir("/tmp/custom-project");
    let mcp_server = sample_mcp_server("docs", "docs-server");
    let skill = sample_skill("lint", "lint");
    let steering = Steering {
        id: SteeringId("host-steering".to_string()),
        scope: SteeringScope::Host,
        title: "Host Rules".to_string(),
        content: "Replay host steering".to_string(),
    };
    let custom_agent = sample_custom_agent(
        "reviewer",
        vec![skill.id.clone()],
        vec![mcp_server.id.clone()],
        ToolPolicy::Unrestricted,
    );

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Custom Project".to_string(),
            roots: vec!["/tmp/custom-project".to_string()],
        })
        .await
        .expect("project_create failed");
    let project = expect_project_notify(&mut fixture.client, "project create").await;

    fixture
        .client
        .mcp_server_upsert(McpServerUpsertPayload {
            mcp_server: mcp_server.clone(),
        })
        .await
        .expect("mcp_server_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "McpServerNotify upsert").await;

    write_skill(fixture.store_dir(), &skill, "Replay skill body");
    fixture
        .client
        .skill_refresh(SkillRefreshPayload::default())
        .await
        .expect("skill_refresh failed");
    let _ = expect_next_event(&mut fixture.client, "SkillNotify upsert").await;

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: steering.clone(),
        })
        .await
        .expect("steering_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "SteeringNotify upsert").await;

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("ordered".to_string()),
            custom_agent_id: Some(custom_agent.id.clone()),
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/replay-order".to_string()],
                prompt: "order".to_string(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn ordered agent failed");
    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    let _ = expect_turn_text(&mut fixture.client, "ordered turn").await;

    let (mut replay, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(bootstrap.projects.iter().any(|item| item.id == project.id));
    assert!(bootstrap.mcp_servers.contains(&mcp_server));
    assert!(bootstrap.skills.contains(&skill));
    assert!(bootstrap.steering.contains(&steering));
    assert!(bootstrap.custom_agents.contains(&custom_agent));
    assert!(bootstrap.agents.iter().any(|agent| agent.name == "ordered"));

    let env = expect_next_event(&mut replay, "ordered agent bootstrap").await;
    assert_eq!(env.kind, FrameKind::AgentStart);
}

#[tokio::test]
async fn spawn_with_custom_agent_resolves_expected_configuration() {
    let mut fixture = Fixture::new().await;
    ensure_dir("/tmp/spawn-project");
    let mcp_server = sample_mcp_server("docs", "docs-server");
    let skill = sample_skill("lint", "lint");
    let host_steering = Steering {
        id: SteeringId("host-zeta".to_string()),
        scope: SteeringScope::Host,
        title: "Zulu".to_string(),
        content: "host steering body".to_string(),
    };
    let custom_agent = sample_custom_agent(
        "reviewer",
        vec![skill.id.clone()],
        vec![mcp_server.id.clone()],
        ToolPolicy::Unrestricted,
    );

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Spawn Project".to_string(),
            roots: vec!["/tmp/spawn-project".to_string()],
        })
        .await
        .expect("project_create failed");
    let project = expect_project_notify(&mut fixture.client, "project create").await;
    let project_steering = Steering {
        id: SteeringId("project-alpha".to_string()),
        scope: SteeringScope::Project(project.id.clone()),
        title: "Alpha".to_string(),
        content: "project steering body".to_string(),
    };

    fixture
        .client
        .mcp_server_upsert(McpServerUpsertPayload {
            mcp_server: mcp_server.clone(),
        })
        .await
        .expect("mcp_server_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "McpServerNotify upsert").await;

    write_skill(
        fixture.store_dir(),
        &skill,
        "Run cargo test -q before reporting completion.",
    );
    fixture
        .client
        .skill_refresh(SkillRefreshPayload::default())
        .await
        .expect("skill_refresh failed");
    let _ = expect_next_event(&mut fixture.client, "SkillNotify upsert").await;

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: host_steering.clone(),
        })
        .await
        .expect("host steering upsert failed");
    let _ = expect_next_event(&mut fixture.client, "host SteeringNotify").await;

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: project_steering.clone(),
        })
        .await
        .expect("project steering upsert failed");
    let _ = expect_next_event(&mut fixture.client, "project SteeringNotify").await;

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("customized".to_string()),
            custom_agent_id: Some(custom_agent.id.clone()),
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/customized".to_string()],
                prompt: "hello".to_string(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
    assert_eq!(new_agent.custom_agent_id, Some(custom_agent.id.clone()));

    let env = expect_next_event(&mut fixture.client, "AgentStart").await;
    let agent_start: AgentStartPayload = env.parse_payload().expect("parse AgentStartPayload");
    assert_eq!(agent_start.custom_agent_id, Some(custom_agent.id.clone()));

    let text = expect_turn_text(&mut fixture.client, "customized turn").await;
    assert!(text.contains("[startup_mcp_servers: tyde-agent-control(http), docs-server(stdio)]"));
    assert!(text.contains("[instructions: reviewer instructions]"));
    assert!(text.contains("[skills: lint=Run cargo test -q before reporting completion.]"));
    assert!(text.contains("[steering: project steering body\\n\\nhost steering body]"));
}

#[tokio::test]
async fn tool_policy_rejection_for_non_claude_backends() {
    let mut fixture = Fixture::new().await;
    let cases = vec![
        (
            "codex-allow",
            BackendKind::Codex,
            ToolPolicy::AllowList {
                tools: vec!["Read".to_string()],
            },
        ),
        (
            "kiro-deny",
            BackendKind::Kiro,
            ToolPolicy::DenyList {
                tools: vec!["Edit".to_string()],
            },
        ),
        (
            "gemini-allow",
            BackendKind::Gemini,
            ToolPolicy::AllowList {
                tools: vec!["Read".to_string()],
            },
        ),
        (
            "tycode-deny",
            BackendKind::Tycode,
            ToolPolicy::DenyList {
                tools: vec!["Edit".to_string()],
            },
        ),
    ];

    for (id, backend_kind, tool_policy) in cases {
        let custom_agent = sample_custom_agent(id, Vec::new(), Vec::new(), tool_policy);
        fixture
            .client
            .custom_agent_upsert(CustomAgentUpsertPayload {
                custom_agent: custom_agent.clone(),
            })
            .await
            .expect("custom_agent_upsert failed");
        let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;

        fixture
            .client
            .spawn_agent(SpawnAgentPayload {
                name: Some(id.to_string()),
                custom_agent_id: Some(custom_agent.id.clone()),
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots: vec!["/tmp/tool-policy".to_string()],
                    prompt: format!("spawn {id}"),
                    images: None,
                    backend_kind,
                    cost_hint: None,
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await
            .expect("spawn_agent should enqueue startup failure");

        let env = expect_next_event(&mut fixture.client, "NewAgent for tool policy failure").await;
        let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
        let _ = expect_next_event(&mut fixture.client, "AgentStart for tool policy failure").await;
        let payload = expect_agent_error_containing(
            &mut fixture.client,
            &new_agent.instance_stream,
            "does not support tool policy",
            "AgentError for tool policy failure",
        )
        .await;
        assert!(payload.fatal);
        assert!(
            payload.message.contains("does not support tool policy"),
            "unexpected tool policy error: {}",
            payload.message
        );
    }
}

#[tokio::test]
async fn resume_re_resolves_deleted_custom_agent_with_warning() {
    let mut fixture = Fixture::new().await;
    let custom_agent = sample_custom_agent(
        "resume-agent",
        Vec::new(),
        Vec::new(),
        ToolPolicy::Unrestricted,
    );

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumable".to_string()),
            custom_agent_id: Some(custom_agent.id.clone()),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/resume-custom".to_string()],
                prompt: "first".to_string(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    let first_turn = expect_turn_text(&mut fixture.client, "initial turn").await;
    assert!(first_turn.contains("[instructions: resume-agent instructions]"));

    fixture
        .client
        .list_sessions(protocol::ListSessionsPayload::default())
        .await
        .expect("list_sessions failed");
    let session_list = wait_for_session_list(&mut fixture.client, "SessionList").await;
    let session = session_list
        .sessions
        .first()
        .cloned()
        .expect("expected stored session");

    fixture
        .client
        .custom_agent_delete(CustomAgentDeletePayload {
            id: custom_agent.id.clone(),
        })
        .await
        .expect("custom_agent_delete failed");
    let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify delete").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("resumed".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::Resume {
                session_id: session.id.clone(),
                prompt: Some("after resume".to_string()),
            },
        })
        .await
        .expect("resume spawn failed");

    let env = expect_next_event(&mut fixture.client, "resumed NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse resumed NewAgentPayload");
    assert_eq!(new_agent.custom_agent_id, None);

    let env = expect_next_event(&mut fixture.client, "resumed AgentStart").await;
    let agent_start: AgentStartPayload = env
        .parse_payload()
        .expect("parse resumed AgentStartPayload");
    assert_eq!(agent_start.custom_agent_id, None);

    let warning = expect_agent_error_containing(
        &mut fixture.client,
        &new_agent.instance_stream,
        "was deleted; resuming without custom agent configuration",
        "resume warning",
    )
    .await;
    assert!(!warning.fatal);
    assert!(
        warning
            .message
            .contains("was deleted; resuming without custom agent configuration"),
        "unexpected warning: {}",
        warning.message
    );

    let resumed_turn = expect_turn_text(&mut fixture.client, "resumed turn").await;
    assert!(!resumed_turn.contains("[instructions:"));
    assert!(resumed_turn.contains("mock backend response to: after resume"));
}

#[tokio::test]
async fn reserved_mcp_name_collision_returns_spawn_error() {
    let mut fixture = Fixture::new().await;
    let reserved_server = sample_mcp_server("debug-alias", "tyde-debug");
    let custom_agent = sample_custom_agent(
        "collision-agent",
        Vec::new(),
        vec![reserved_server.id.clone()],
        ToolPolicy::Unrestricted,
    );

    fixture
        .client
        .mcp_server_upsert(McpServerUpsertPayload {
            mcp_server: reserved_server.clone(),
        })
        .await
        .expect("mcp_server_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "McpServerNotify upsert").await;

    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    let _ = expect_next_event(&mut fixture.client, "CustomAgentNotify upsert").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("collision".to_string()),
            custom_agent_id: Some(custom_agent.id.clone()),
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/collision".to_string()],
                prompt: "collision".to_string(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent should enqueue startup failure");

    let env = expect_next_event(&mut fixture.client, "NewAgent").await;
    let new_agent: NewAgentPayload = env.parse_payload().expect("parse NewAgentPayload");
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    let payload = expect_agent_error_containing(
        &mut fixture.client,
        &new_agent.instance_stream,
        "reserved MCP server name 'tyde-debug'",
        "collision AgentError",
    )
    .await;
    assert!(payload.fatal);
    assert!(
        payload
            .message
            .contains("reserved MCP server name 'tyde-debug'")
    );
}

#[tokio::test]
async fn steering_ordering_combines_host_and_project_by_title() {
    let mut fixture = Fixture::new().await;
    ensure_dir("/tmp/ordering-project");
    let host_steering = Steering {
        id: SteeringId("host-zulu".to_string()),
        scope: SteeringScope::Host,
        title: "Zulu".to_string(),
        content: "host zulu".to_string(),
    };

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Ordering Project".to_string(),
            roots: vec!["/tmp/ordering-project".to_string()],
        })
        .await
        .expect("project_create failed");
    let project = expect_project_notify(&mut fixture.client, "project create").await;
    let project_steering = Steering {
        id: SteeringId("project-alpha".to_string()),
        scope: SteeringScope::Project(project.id.clone()),
        title: "Alpha".to_string(),
        content: "project alpha".to_string(),
    };

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: host_steering,
        })
        .await
        .expect("host steering upsert failed");
    let _ = expect_next_event(&mut fixture.client, "host SteeringNotify").await;

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: project_steering,
        })
        .await
        .expect("project steering upsert failed");
    let _ = expect_next_event(&mut fixture.client, "project SteeringNotify").await;

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("ordered-steering".to_string()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec!["/tmp/ordered-steering".to_string()],
                prompt: "steer".to_string(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    let text = expect_turn_text(&mut fixture.client, "steering turn").await;
    assert!(text.contains("[steering: project alpha\\n\\nhost zulu]"));
}
