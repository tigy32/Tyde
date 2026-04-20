mod fixture;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentErrorPayload, AgentStartPayload, BackendKind, CustomAgent, CustomAgentDeletePayload,
    CustomAgentId, CustomAgentNotifyPayload, CustomAgentUpsertPayload, Envelope, FrameKind,
    McpServerConfig, McpServerDeletePayload, McpServerId, McpServerNotifyPayload,
    McpServerUpsertPayload, McpTransportConfig, NewAgentPayload, ProjectCreatePayload,
    ProjectNotifyPayload, Skill, SkillId, SkillNotifyPayload, SkillRefreshPayload,
    SpawnAgentParams, SpawnAgentPayload, Steering, SteeringDeletePayload, SteeringId,
    SteeringNotifyPayload, SteeringScope, SteeringUpsertPayload, ToolPolicy,
};
use serde_json::to_string_pretty;

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::SessionList
        ) {
            continue;
        }
        return env;
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
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
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

    let mut replay = fixture.connect_fresh_host().await;
    let mut saw_mcp = false;
    let mut saw_skill = false;
    let mut saw_steering = false;
    let mut saw_custom_agent = false;
    while !(saw_mcp && saw_skill && saw_steering && saw_custom_agent) {
        let env = expect_next_event(&mut replay, "customization replay").await;
        match env.kind {
            FrameKind::McpServerNotify => {
                saw_mcp = true;
                assert_eq!(
                    env.parse_payload::<McpServerNotifyPayload>()
                        .expect("parse replay McpServerNotifyPayload"),
                    McpServerNotifyPayload::Upsert {
                        mcp_server: mcp_server.clone()
                    }
                );
            }
            FrameKind::SkillNotify => {
                saw_skill = true;
                assert_eq!(
                    env.parse_payload::<SkillNotifyPayload>()
                        .expect("parse replay SkillNotifyPayload"),
                    SkillNotifyPayload::Upsert {
                        skill: skill.clone()
                    }
                );
            }
            FrameKind::SteeringNotify => {
                saw_steering = true;
                assert_eq!(
                    env.parse_payload::<SteeringNotifyPayload>()
                        .expect("parse replay SteeringNotifyPayload"),
                    SteeringNotifyPayload::Upsert {
                        steering: steering.clone()
                    }
                );
            }
            FrameKind::CustomAgentNotify => {
                saw_custom_agent = true;
                assert_eq!(
                    env.parse_payload::<CustomAgentNotifyPayload>()
                        .expect("parse replay CustomAgentNotifyPayload"),
                    CustomAgentNotifyPayload::Upsert {
                        custom_agent: custom_agent.clone()
                    }
                );
            }
            other => panic!("unexpected replay event: {other:?}"),
        }
    }

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
                session_settings: None,
            },
        })
        .await
        .expect("spawn ordered agent failed");
    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    let _ = expect_turn_text(&mut fixture.client, "ordered turn").await;

    let mut replay = fixture.connect().await;
    let mut observed = Vec::new();
    while !observed.contains(&FrameKind::AgentStart) {
        let env = expect_next_event(&mut replay, "ordered replay").await;
        match env.kind {
            FrameKind::ProjectNotify
            | FrameKind::McpServerNotify
            | FrameKind::SkillNotify
            | FrameKind::SteeringNotify
            | FrameKind::CustomAgentNotify
            | FrameKind::NewAgent
            | FrameKind::AgentStart => observed.push(env.kind),
            FrameKind::ChatEvent => {}
            other => panic!("unexpected replay event kind {other:?}"),
        }
    }

    assert_eq!(
        observed,
        vec![
            FrameKind::ProjectNotify,
            FrameKind::McpServerNotify,
            FrameKind::SkillNotify,
            FrameKind::SteeringNotify,
            FrameKind::CustomAgentNotify,
            FrameKind::NewAgent,
            FrameKind::AgentStart,
        ]
    );
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
                    session_settings: None,
                },
            })
            .await
            .expect("spawn_agent should enqueue startup failure");

        let _ = expect_next_event(&mut fixture.client, "NewAgent for tool policy failure").await;
        let _ = expect_next_event(&mut fixture.client, "AgentStart for tool policy failure").await;
        let env =
            expect_next_event(&mut fixture.client, "AgentError for tool policy failure").await;
        assert_eq!(env.kind, FrameKind::AgentError);
        let payload: AgentErrorPayload = env.parse_payload().expect("parse AgentErrorPayload");
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

    let env = expect_next_event(&mut fixture.client, "resume warning").await;
    assert_eq!(env.kind, FrameKind::AgentError);
    let warning: AgentErrorPayload = env
        .parse_payload()
        .expect("parse warning AgentErrorPayload");
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
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent should enqueue startup failure");

    let _ = expect_next_event(&mut fixture.client, "NewAgent").await;
    let _ = expect_next_event(&mut fixture.client, "AgentStart").await;
    let env = expect_next_event(&mut fixture.client, "collision AgentError").await;
    let payload: AgentErrorPayload = env
        .parse_payload()
        .expect("parse collision AgentErrorPayload");
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
