mod fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fixture::{Fixture, next_frame_matching_on};
use protocol::{
    AgentId, AgentStartPayload, BackendAccessMode, BackendKind, CommandErrorCode,
    CommandErrorPayload, CustomAgent, CustomAgentId, CustomAgentUpsertPayload, Envelope, FrameKind,
    GitBranchName, NewAgentPayload, NewTerminalPayload, Project, ProjectAddRootPayload,
    ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload, ProjectId,
    ProjectNotifyPayload, ProjectRootPath, ProjectSource, SpawnAgentParams, SpawnAgentPayload,
    Steering, SteeringId, SteeringScope, SteeringUpsertPayload, TeamCreatePayload,
    TeamMemberCreateSpec, TerminalCreatePayload, TerminalLaunchTarget, ToolPolicy,
    WorkbenchCreatePayload, WorkbenchRemovePayload,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    next_frame_matching_on(client, context, |env| {
        !fixture::is_routine_control_plane_frame(env)
            && !matches!(
                env.kind,
                FrameKind::HostSettings
                    | FrameKind::BackendCapacity
                    | FrameKind::SessionList
                    | FrameKind::TaskTokenUsage
                    | FrameKind::WorkflowNotify
                    | FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::AgentActivityStats
                    | FrameKind::AgentActivityChanged
                    | FrameKind::CustomAgentNotify
                    | FrameKind::SteeringNotify
                    | FrameKind::SkillNotify
                    | FrameKind::McpServerNotify
                    | FrameKind::ProjectBootstrap
                    | FrameKind::ProjectEvent
                    | FrameKind::ProjectFileList
                    | FrameKind::ProjectGitStatus
                    | FrameKind::CodeIntelOverview
                    | FrameKind::ChatEvent
                    | FrameKind::ContextCompactionCapability
                    | FrameKind::AgentBootstrap
                    | FrameKind::AgentStart
                    | FrameKind::NewAgent
                    | FrameKind::TerminalBootstrap
                    | FrameKind::TerminalStart
                    | FrameKind::NewTerminal
                    | FrameKind::TerminalOutput
                    | FrameKind::TerminalExit
                    | FrameKind::TerminalError
                    | FrameKind::AgentError
                    | FrameKind::AgentClosed
            )
    })
    .await
}

async fn expect_kind(client: &mut client::Connection, kind: FrameKind, context: &str) -> Envelope {
    fixture::next_logical_frame_matching_on(client, context, |env| env.kind == kind).await
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    let env = expect_next_event(client, context).await;
    match env.kind {
        FrameKind::ProjectNotify => env.parse_payload().unwrap_or_else(|error| {
            panic!(
                "failed to parse ProjectNotifyPayload: context={context}, stream={}, kind={:?}, error={error}",
                env.stream, env.kind
            )
        }),
        FrameKind::CommandError => {
            let error: CommandErrorPayload = env.parse_payload().unwrap_or_else(|parse_error| {
                panic!(
                    "failed to parse CommandErrorPayload while waiting for ProjectNotify: context={context}, stream={}, kind={:?}, error={parse_error}",
                    env.stream, env.kind
                )
            });
            panic!(
                "expected ProjectNotify, got CommandError: context={context}, envelope_stream={}, stream={}, request_kind={:?}, operation={}, code={:?}, message={:?}, fatal={}",
                env.stream,
                error.stream,
                error.request_kind,
                error.operation,
                error.code,
                error.message,
                error.fatal
            );
        }
        kind => panic!(
            "expected ProjectNotify: context={context}, stream={}, kind={kind:?}",
            env.stream
        ),
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

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {:?}: {}", args, err));
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("git output UTF-8")
        .trim()
        .to_owned()
}

async fn call_agent_control(
    fixture: &Fixture,
    caller: &AgentId,
    name: &str,
    arguments: Value,
) -> (bool, String) {
    call_agent_control_optional(fixture, Some(caller), name, arguments).await
}

async fn call_agent_control_optional(
    fixture: &Fixture,
    caller: Option<&AgentId>,
    name: &str,
    arguments: Value,
) -> (bool, String) {
    let url = if name == "tyde_review" {
        fixture
            .agent_control_caller(caller.expect("review caller"))
            .await
            .await_url
    } else {
        fixture.agent_control_http_url().await
    };
    let bearer = match caller {
        Some(caller) => {
            let caller_auth = fixture.agent_control_caller(caller).await;
            Some(
                caller_auth
                    .authorization
                    .strip_prefix("Bearer ")
                    .expect("bearer credential")
                    .to_owned(),
            )
        }
        None => None,
    };
    call_agent_control_request(url, bearer, name.to_owned(), arguments).await
}

async fn call_agent_control_request(
    url: String,
    bearer: Option<String>,
    name: String,
    arguments: Value,
) -> (bool, String) {
    let config = StreamableHttpClientTransportConfig::with_uri(url);
    let config = match bearer.as_deref() {
        Some(bearer) => config.auth_header(bearer),
        None => config,
    };
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = ().serve(transport).await.expect("connect agent-control MCP");
    let result = service
        .call_tool(CallToolRequestParams {
            meta: None,
            name: name.clone().into(),
            arguments: Some(arguments.as_object().expect("object arguments").clone()),
            task: None,
        })
        .await
        .expect("call agent-control tool");
    let RawContent::Text(text) = &result.content.first().expect("tool content").raw else {
        panic!("expected text tool result")
    };
    let response = (result.is_error.unwrap_or(false), text.text.clone());
    service.cancel().await.expect("cancel MCP client");
    response
}

async fn assert_no_new_agent(client: &mut client::Connection, context: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    loop {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            return;
        };
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) if env.kind == FrameKind::NewAgent => {
                panic!("unexpected NewAgent while {context}")
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => panic!("connection closed while {context}"),
            Ok(Err(error)) => panic!("event error while {context}: {error:?}"),
            Err(_) => return,
        }
    }
}

async fn assert_no_project_upsert(client: &mut client::Connection, context: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    loop {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            return;
        };
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) if env.kind == FrameKind::ProjectNotify => {
                let notify: ProjectNotifyPayload = env.parse_payload().expect("ProjectNotify");
                if matches!(notify, ProjectNotifyPayload::Upsert { .. }) {
                    panic!("unexpected project upsert while {context}");
                }
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => panic!("connection closed while {context}"),
            Ok(Err(error)) => panic!("event error while {context}: {error:?}"),
            Err(_) => return,
        }
    }
}

fn init_git_repo(name: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("create temp repo");
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "tests@example.com"]);
    git(repo.path(), &["config", "user.name", "Tests"]);
    fs::write(repo.path().join("README.md"), format!("# {name}\n")).expect("write readme");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", name]);
    repo
}

async fn create_project(client: &mut client::Connection, roots: Vec<&Path>) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: "Parent".to_owned(),
            roots: roots
                .into_iter()
                .map(|root| ProjectRootPath(root.to_string_lossy().to_string()))
                .collect(),
        })
        .await
        .expect("project_create failed");
    match expect_project_notify(client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected project upsert, got {other:?}"),
    }
}

async fn create_workbench(
    client: &mut client::Connection,
    parent: &Project,
    branch: &str,
) -> Project {
    client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName(branch.to_owned()),
            name: branch.to_owned(),
        })
        .await
        .expect("workbench_create write failed");
    match expect_project_notify(client, "workbench create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected workbench upsert, got {other:?}"),
    }
}

fn project_roots(project: &Project) -> Vec<String> {
    project
        .root_paths()
        .into_iter()
        .map(|root| root.0)
        .collect()
}

fn expected_worktree_path(parent_root: &Path, encoded_branch: &str) -> PathBuf {
    let basename = parent_root
        .file_name()
        .and_then(|name| name.to_str())
        .expect("temp repo basename should be UTF-8");
    parent_root.with_file_name(format!("{}--{}", basename, encoded_branch))
}

async fn spawn_project_caller(
    client: &mut client::Connection,
    project: &Project,
    name: &str,
    access_mode: BackendAccessMode,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(project),
                prompt: "workbench caller".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode,
                session_settings: None,
            },
        })
        .await
        .expect("spawn project caller");
    let mut caller = None;
    next_frame_matching_on(client, name, |env| {
        if env.kind != FrameKind::NewAgent {
            return false;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("NewAgent");
        if payload.name == name {
            caller = Some(payload);
            true
        } else {
            false
        }
    })
    .await;
    caller.expect("project caller NewAgent")
}

#[tokio::test]
async fn workbench_create_and_remove_round_trip_real_git_repo() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("round-trip");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;

    let workbench = create_workbench(&mut fixture.client, &parent, "feature/login").await;
    let expected_path = expected_worktree_path(repo.path(), "feature-login");

    match &workbench.source {
        ProjectSource::GitWorkbench {
            parent_project_id,
            branch,
            roots,
            ..
        } => {
            assert_eq!(parent_project_id, &parent.id);
            assert_eq!(branch.0, "feature/login");
            assert_eq!(roots.len(), 1);
            assert_eq!(roots[0].parent_root.0, repo.path().to_string_lossy());
            assert_eq!(roots[0].worktree_root.0, expected_path.to_string_lossy());
        }
        other => panic!("expected GitWorkbench source, got {other:?}"),
    }
    assert!(expected_path.is_dir());
    let branch = Command::new("git")
        .arg("-C")
        .arg(&expected_path)
        .args(["branch", "--show-current"])
        .output()
        .expect("run git branch");
    assert!(branch.status.success());
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "feature/login"
    );

    let first = fixture.spawn("same connection chat", "initial").await;
    fixture.finish_turn(&first).await;
    let second = fixture.spawn("second connection chat", "initial").await;
    fixture.finish_turn(&second).await;
    let (mut observer, bootstrap) = fixture.connect_with_bootstrap().await;
    let second_stream = bootstrap
        .agents
        .iter()
        .find(|agent| agent.agent_id == second.new_agent.agent_id)
        .unwrap()
        .instance_stream
        .clone();
    let hook = fixture.install_workbench_remove_test_hook();
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    hook.wait_until_reached().await;
    eprintln!("PROJECT MUTATION EVIDENCE: removal held before Git/cleanup; release is closed");
    fixture
        .mock(&second)
        .await
        .enqueue(MockTurn::text("second client completed while held"))
        .await;
    observer
        .send_message(&second_stream, "independent chat".into())
        .await
        .unwrap();
    fixture::next_logical_frame_matching_on(&mut observer, "second-client chat while removal held", |env| {
        env.stream == second_stream && env.kind == FrameKind::ChatEvent && matches!(env.parse_payload::<protocol::ChatEvent>().unwrap(), protocol::ChatEvent::StreamEnd(end) if end.message.content == "second client completed while held")
    }).await;
    eprintln!(
        "PROJECT MUTATION EVIDENCE: second client chat completed; same-connection heartbeat is next while release remains closed"
    );
    fixture
        .client
        .heartbeat(protocol::HeartbeatPayload {
            client_sent_at_ms: 7,
        })
        .await
        .unwrap();
    let heartbeat = expect_kind(
        &mut fixture.client,
        FrameKind::HeartbeatAck,
        "same-connection heartbeat while removal held",
    )
    .await;
    assert_eq!(
        heartbeat
            .parse_payload::<protocol::HeartbeatPayload>()
            .unwrap()
            .client_sent_at_ms,
        7
    );
    fixture
        .client
        .list_sessions(protocol::ListSessionsPayload::default())
        .await
        .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::SessionList,
        "same-connection command while removal held",
    )
    .await;
    fixture
        .client
        .project_read_file(
            &parent.id,
            protocol::ProjectReadFilePayload {
                path: protocol::ProjectPath {
                    root: parent.root_paths()[0].clone(),
                    relative_path: "README.md".into(),
                },
            },
        )
        .await
        .unwrap();
    let read = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectFileContents,
        "same-connection read while removal held",
    )
    .await
    .parse_payload::<protocol::ProjectFileContentsPayload>()
    .unwrap();
    assert_eq!(read.contents.as_deref(), Some("# round-trip\n"));
    fixture
        .mock(&first)
        .await
        .enqueue(MockTurn::text("same client completed while held"))
        .await;
    fixture
        .client
        .send_message(&first.stream, "same-connection chat".into())
        .await
        .unwrap();
    fixture::next_logical_frame_matching_on(&mut fixture.client, "same-connection chat while removal held", |env| {
        env.stream == first.stream && env.kind == FrameKind::ChatEvent && matches!(env.parse_payload::<protocol::ChatEvent>().unwrap(), protocol::ChatEvent::StreamEnd(end) if end.message.content == "same client completed while held")
    }).await;
    assert!(
        expected_path.is_dir(),
        "held operation must still be unfinished"
    );
    eprintln!(
        "PROJECT MUTATION EVIDENCE: both chats, same-connection heartbeat/command/read completed before release"
    );
    observer
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("conflicting-operation".into()),
            name: "conflicting operation".into(),
        })
        .await
        .unwrap();
    let conflict = expect_command_error(&mut observer, "held catalog family conflict").await;
    assert_eq!(conflict.code, CommandErrorCode::Conflict);
    assert!(
        matches!(conflict.context, Some(protocol::CommandErrorContext::WorkbenchCreate { parent_project_id, .. }) if parent_project_id == parent.id)
    );

    let (mut reconnect, _) = fixture.connect_with_bootstrap().await;
    let replay = next_frame_matching_on(&mut reconnect, "current operation replay", |env| {
        env.kind == FrameKind::ProjectOperation && matches!(env.parse_payload::<protocol::ProjectOperationPayload>().unwrap().mutation, protocol::ProjectMutation::RemoveWorkbench(payload) if payload.id == workbench.id)
    }).await.parse_payload::<protocol::ProjectOperationPayload>().unwrap();
    assert_eq!(replay.host_id.0, protocol::LOCAL_HOST_ID);
    assert!(matches!(
        replay.state,
        protocol::ProjectOperationState::Admitted | protocol::ProjectOperationState::Running { .. }
    ));
    let (mut reconnect_again, _) = fixture.connect_with_bootstrap().await;
    let second_replay = next_frame_matching_on(
        &mut reconnect_again,
        "same current operation on reconnect",
        |env| {
            env.kind == FrameKind::ProjectOperation
                && env
                    .parse_payload::<protocol::ProjectOperationPayload>()
                    .unwrap()
                    .stream
                    == replay.stream
        },
    )
    .await
    .parse_payload::<protocol::ProjectOperationPayload>()
    .unwrap();
    assert!(matches!(
        second_replay.state,
        protocol::ProjectOperationState::Admitted | protocol::ProjectOperationState::Running { .. }
    ));
    drop(reconnect_again);
    fixture.client = reconnect;
    eprintln!(
        "PROJECT MUTATION EVIDENCE: conflicting admission rejected; reconnect replays current operation; submitting connection disconnected before release"
    );
    hook.resume();
    match expect_project_notify(&mut fixture.client, "workbench remove").await {
        ProjectNotifyPayload::Delete { project } => assert_eq!(project, workbench),
        other => panic!("expected workbench delete, got {other:?}"),
    }
    assert!(!expected_path.exists());
    let terminal =
        next_frame_matching_on(&mut fixture.client, "committed resource operation", |env| {
            env.kind == FrameKind::ProjectOperation
                && matches!(env.parse_payload::<protocol::ProjectOperationPayload>().unwrap(), protocol::ProjectOperationPayload { stream, state: protocol::ProjectOperationState::Committed { .. } | protocol::ProjectOperationState::Failed { .. }, .. } if stream == replay.stream)
        })
        .await
        .parse_payload::<protocol::ProjectOperationPayload>()
        .unwrap();
    assert!(
        matches!(terminal.state, protocol::ProjectOperationState::Committed { warnings } if warnings.is_empty())
    );
    let (mut settled, _) = fixture.connect_with_bootstrap().await;
    settled
        .heartbeat(protocol::HeartbeatPayload {
            client_sent_at_ms: 9,
        })
        .await
        .unwrap();
    loop {
        let env = settled
            .next_event()
            .await
            .unwrap()
            .expect("settled bootstrap connection");
        if env.kind == FrameKind::ProjectOperation {
            let operation: protocol::ProjectOperationPayload = env.parse_payload().unwrap();
            assert_ne!(
                operation.stream, replay.stream,
                "bootstrap must not replay terminal history"
            );
        }
        if env.kind == FrameKind::HeartbeatAck {
            break;
        }
    }
    eprintln!(
        "PROJECT MUTATION EVIDENCE: release completed real Git removal and typed commit; settled bootstrap contains no terminal history"
    );
}

#[tokio::test]
async fn workbench_create_rejects_path_collision_and_existing_branch() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("create-failures");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;

    let collision_path = expected_worktree_path(repo.path(), "feature-collision");
    fs::create_dir(&collision_path).expect("create collision path");
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("feature/collision".to_owned()),
            name: "feature/collision".to_owned(),
        })
        .await
        .expect("workbench_create write failed");
    let error = expect_command_error(&mut fixture.client, "path collision").await;
    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("already exists"));

    git(repo.path(), &["branch", "already-there"]);
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("already-there".to_owned()),
            name: "already-there".to_owned(),
        })
        .await
        .expect("workbench_create write failed");
    let error = expect_command_error(&mut fixture.client, "existing branch").await;
    assert!(
        matches!(&error.context, Some(protocol::CommandErrorContext::WorkbenchCreate { parent_project_id, branch })
        if parent_project_id == &parent.id && branch.0 == "already-there")
    );
    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("already exists"));

    let registered_path = expected_worktree_path(repo.path(), "feature-registered");
    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Other project".to_owned(),
            roots: vec![ProjectRootPath(
                registered_path.to_string_lossy().to_string(),
            )],
        })
        .await
        .expect("project_create for registered path failed");
    match expect_project_notify(&mut fixture.client, "registered path project create").await {
        ProjectNotifyPayload::Upsert { .. } => {}
        other => panic!("expected project upsert, got {other:?}"),
    }
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("feature/registered".to_owned()),
            name: "feature/registered".to_owned(),
        })
        .await
        .expect("workbench_create write failed");
    let error = expect_command_error(&mut fixture.client, "registered path collision").await;
    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("already registered"));

    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("bad branch".to_owned()),
            name: "bad branch".to_owned(),
        })
        .await
        .expect("workbench_create write failed");
    let error = expect_command_error(&mut fixture.client, "invalid branch").await;
    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
}

#[tokio::test]
async fn workbench_remove_rejects_dirty_worktree_unless_forced() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("dirty-remove");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let workbench = create_workbench(&mut fixture.client, &parent, "dirty-remove").await;
    let worktree_root = PathBuf::from(project_roots(&workbench)[0].clone());
    fs::write(worktree_root.join("dirty.txt"), "dirty\n").expect("write dirty file");

    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    let mut failed = None;
    let mut error = None;
    while failed.is_none() || error.is_none() {
        let env = fixture
            .client
            .next_event()
            .await
            .unwrap()
            .expect("dirty removal outcome");
        if env.kind == FrameKind::ProjectOperation {
            let operation: protocol::ProjectOperationPayload = env.parse_payload().unwrap();
            if matches!(&operation.mutation, protocol::ProjectMutation::RemoveWorkbench(payload) if payload.id == workbench.id)
                && matches!(
                    operation.state,
                    protocol::ProjectOperationState::Failed { .. }
                )
            {
                failed = Some(operation);
            }
        } else if env.kind == FrameKind::CommandError {
            error = Some(env.parse_payload::<CommandErrorPayload>().unwrap());
        }
    }
    assert!(matches!(
        failed.unwrap().state,
        protocol::ProjectOperationState::Failed {
            code: CommandErrorCode::Conflict,
            cleanup_may_be_partial: false,
            ..
        }
    ));
    let error = error.unwrap();
    assert!(
        matches!(&error.context, Some(protocol::CommandErrorContext::WorkbenchRemove { project_id }) if project_id == &workbench.id)
    );
    assert_eq!(error.operation, "workbench_remove");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(
        error
            .message
            .contains(worktree_root.to_string_lossy().as_ref())
    );
    assert!(error.message.contains("?? dirty.txt"));
    assert!(worktree_root.join("dirty.txt").exists());

    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
            force: true,
        })
        .await
        .expect("forced workbench_remove write failed");
    let deleted: ProjectNotifyPayload =
        expect_project_notify(&mut fixture.client, "forced dirty workbench remove").await;
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == workbench.id)
    );
    assert!(!worktree_root.exists());
}

#[tokio::test]
async fn workbench_blocks_parent_mutations_and_project_delete_on_workbench() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("parent-blockers");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let workbench = create_workbench(&mut fixture.client, &parent, "parent-blockers").await;

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: parent.id.clone(),
        })
        .await
        .expect("project_delete write failed");
    let error = expect_command_error(&mut fixture.client, "parent delete blocked").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("referenced by workbench"));

    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: parent.id.clone(),
            root: ProjectRootPath("/tmp/new-parent-root".to_owned()),
        })
        .await
        .expect("project_add_root write failed");
    let error = expect_command_error(&mut fixture.client, "parent add root blocked").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("referenced by workbench"));

    fixture
        .client
        .project_delete_root(ProjectDeleteRootPayload {
            id: parent.id.clone(),
            root: parent.root_paths()[0].clone(),
        })
        .await
        .expect("project_delete_root write failed");
    let error = expect_command_error(&mut fixture.client, "parent delete root blocked").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("referenced by workbench"));

    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: workbench.id.clone(),
            root: ProjectRootPath("/tmp/new-workbench-root".to_owned()),
        })
        .await
        .expect("project_add_root workbench write failed");
    let error = expect_command_error(&mut fixture.client, "workbench add root blocked").await;
    assert_eq!(error.operation, "project_add_root");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);

    fixture
        .client
        .project_delete_root(ProjectDeleteRootPayload {
            id: workbench.id.clone(),
            root: workbench.root_paths()[0].clone(),
        })
        .await
        .expect("project_delete_root workbench write failed");
    let error = expect_command_error(&mut fixture.client, "workbench delete root blocked").await;
    assert_eq!(error.operation, "project_delete_root");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: workbench.id.clone(),
        })
        .await
        .expect("project_delete workbench write failed");
    let error = expect_command_error(&mut fixture.client, "project delete workbench").await;
    assert_eq!(error.operation, "project_delete");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
}

#[tokio::test]
async fn workbench_create_serializes_same_parent_same_branch() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("serialized-create");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let mut client_a = fixture.connect().await;
    let mut client_b = fixture.connect().await;

    let payload = WorkbenchCreatePayload {
        parent_project_id: parent.id.clone(),
        branch: GitBranchName("serialized-create".to_owned()),
        name: "serialized-create".to_owned(),
    };
    let (write_a, write_b) = tokio::join!(
        client_a.workbench_create(payload.clone()),
        client_b.workbench_create(payload)
    );
    write_a.expect("client A workbench_create write failed");
    write_b.expect("client B workbench_create write failed");

    let error = tokio::select! {
        error = expect_kind(&mut client_a, FrameKind::CommandError, "client A concurrent workbench conflict") => error,
        error = expect_kind(&mut client_b, FrameKind::CommandError, "client B concurrent workbench conflict") => error,
    }.parse_payload::<CommandErrorPayload>().unwrap();
    let created =
        match expect_project_notify(&mut fixture.client, "concurrent workbench create").await {
            ProjectNotifyPayload::Upsert { project } => project,
            other => panic!("expected workbench upsert, got {other:?}"),
        };
    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(
        matches!(error.context, Some(protocol::CommandErrorContext::WorkbenchCreate { parent_project_id, branch }) if parent_project_id == parent.id && branch.0 == "serialized-create")
    );
    assert_eq!(created.parent_project_id(), Some(&parent.id));
    assert!(expected_worktree_path(repo.path(), "serialized-create").is_dir());
}

#[tokio::test]
async fn workbench_create_rolls_back_previously_created_roots_on_late_git_failure() {
    let mut fixture = Fixture::new().await;
    let repo_a = init_git_repo("rollback-a");
    let repo_b = init_git_repo("rollback-b");
    let parent = create_project(&mut fixture.client, vec![repo_a.path(), repo_b.path()]).await;
    let worktree_a = expected_worktree_path(repo_a.path(), "rollback-test");
    let heads_b = repo_b.path().join(".git/refs/heads");

    let original_perms = fs::metadata(&heads_b)
        .expect("stat refs heads")
        .permissions();
    let mut readonly_perms = original_perms.clone();
    readonly_perms.set_readonly(true);
    fs::set_permissions(&heads_b, readonly_perms).expect("make refs heads readonly");

    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("rollback-test".to_owned()),
            name: "rollback-test".to_owned(),
        })
        .await
        .expect("workbench_create write failed");
    let error = expect_command_error(&mut fixture.client, "rollback workbench create").await;

    fs::set_permissions(&heads_b, original_perms).expect("restore refs heads permissions");

    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::Internal);
    assert!(!worktree_a.exists(), "first worktree should be rolled back");

    // Rollback must also delete the branch that `git worktree add -b`
    // created in repo A, otherwise retrying the identical create fails
    // the branch-exists preflight forever.
    let branch_check = Command::new("git")
        .arg("-C")
        .arg(repo_a.path())
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/rollback-test",
        ])
        .output()
        .expect("run git rev-parse for rolled-back branch");
    assert!(
        !branch_check.status.success(),
        "rollback should delete the branch created in repo A"
    );

    // With the branch gone, retrying the same create succeeds.
    let workbench = create_workbench(&mut fixture.client, &parent, "rollback-test").await;
    assert!(worktree_a.is_dir(), "retried create should make worktree A");
    assert!(
        expected_worktree_path(repo_b.path(), "rollback-test").is_dir(),
        "retried create should make worktree B"
    );
    match &workbench.source {
        ProjectSource::GitWorkbench { branch, .. } => assert_eq!(branch.0, "rollback-test"),
        other => panic!("expected GitWorkbench source, got {other:?}"),
    }
}

#[tokio::test]
async fn workbench_remove_succeeds_when_worktree_dir_was_deleted_out_of_band() {
    for residual_directory in [false, true] {
        let mut fixture = Fixture::new().await;
        let repo = init_git_repo("missing-worktree");
        let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
        let workbench = create_workbench(&mut fixture.client, &parent, "missing-worktree").await;
        let worktree_root = PathBuf::from(project_roots(&workbench)[0].clone());
        fs::remove_dir_all(&worktree_root).expect("remove worktree dir out of band");

        if residual_directory {
            fs::create_dir(&worktree_root).expect("recreate residual directory");
            fs::write(worktree_root.join("keep.txt"), "unsaved work").expect("write residual file");
        }

        // A worktree dir deleted out of band (manual `rm -rf`, or a half-failed
        // earlier removal) must not brick the record: removal succeeds, prunes
        // git's worktree bookkeeping, and deletes the store record.
        fixture
            .client
            .workbench_remove(WorkbenchRemovePayload {
                id: workbench.id.clone(),
                force: false,
            })
            .await
            .expect("workbench_remove write failed");
        let expected_stream = protocol::StreamPath(format!("/project/{}", workbench.id));
        let deleted_root = worktree_root.to_string_lossy();
        let mut tolerated_watcher_error = false;
        // The watcher can report the removed root before the host-stream delete.
        // Accept only that source-confirmed project-stream failure and still
        // require the matching delete notification.
        loop {
            let env = expect_next_event(&mut fixture.client, "missing worktree remove").await;
            match env.kind {
                FrameKind::ProjectNotify => {
                    let notify: ProjectNotifyPayload = env
                        .parse_payload()
                        .expect("parse ProjectNotifyPayload for missing worktree remove");
                    match notify {
                        ProjectNotifyPayload::Delete { project } if project.id == workbench.id => {
                            break;
                        }
                        other => panic!(
                            "expected matching workbench delete: context=missing worktree remove, stream={}, kind={:?}, payload={other:?}",
                            env.stream, env.kind
                        ),
                    }
                }
                FrameKind::CommandError => {
                    let error: CommandErrorPayload = env
                        .parse_payload()
                        .expect("parse CommandErrorPayload for missing worktree remove");
                    assert!(
                        !tolerated_watcher_error,
                        "received duplicate watcher error: context=missing worktree remove, envelope_stream={}, stream={}, request_kind={:?}, operation={}, code={:?}, message={:?}, fatal={}",
                        env.stream,
                        error.stream,
                        error.request_kind,
                        error.operation,
                        error.code,
                        error.message,
                        error.fatal
                    );
                    assert_eq!(env.stream, expected_stream, "unexpected envelope stream");
                    assert_eq!(error.stream, expected_stream, "unexpected payload stream");
                    assert_eq!(error.request_kind, FrameKind::ProjectFileList);
                    assert_eq!(error.operation, "project_watch");
                    assert_eq!(error.code, CommandErrorCode::Internal);
                    // Watcher startup reports a recoverable missing-root warning;
                    // a later refresh can instead terminate the project stream.
                    if !error.fatal {
                        assert!(error.message.ends_with(
                            "The project remains available; automatic recovery will retry in a few seconds."
                        ), "nonfatal watcher error must describe automatic recovery");
                    }
                    assert!(
                        error.message.contains(deleted_root.as_ref()),
                        "watcher error must name the exact deleted worktree path: path={deleted_root:?}, message={:?}",
                        error.message
                    );
                    tolerated_watcher_error = true;
                }
                kind => panic!(
                    "expected workbench delete: context=missing worktree remove, stream={}, kind={kind:?}",
                    env.stream
                ),
            }
        }

        let worktrees = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("run git worktree list");
        assert!(worktrees.status.success());
        assert!(
            !String::from_utf8_lossy(&worktrees.stdout)
                .contains(worktree_root.to_string_lossy().as_ref()),
            "git worktree bookkeeping should be pruned for the missing worktree"
        );
        if residual_directory {
            assert_eq!(
                fs::read_to_string(worktree_root.join("keep.txt"))
                    .expect("preserved residual file"),
                "unsaved work"
            );
        }
    }
}

#[tokio::test]
async fn workbench_remove_cascades_team_member_reference() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("team-blocker");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let workbench = create_workbench(&mut fixture.client, &parent, "team-blocker").await;

    fixture
        .client
        .replace_setting(
            "/enabled_backends",
            vec![BackendKind::Claude],
            Vec::<BackendKind>::new(),
        )
        .await
        .expect("set enabled backends failed");
    let custom_agent = CustomAgent {
        id: CustomAgentId("workbench-team-agent".to_owned()),
        name: "Workbench team agent".to_owned(),
        description: "workbench team agent".to_owned(),
        instructions: None,
        skill_ids: Vec::new(),
        mcp_server_ids: Vec::new(),
        tool_policy: ToolPolicy::Unrestricted,
    };
    fixture
        .client
        .custom_agent_upsert(CustomAgentUpsertPayload {
            custom_agent: custom_agent.clone(),
        })
        .await
        .expect("custom_agent_upsert failed");
    fixture
        .client
        .team_create(TeamCreatePayload {
            name: "Workbench team".to_owned(),
            manager: TeamMemberCreateSpec {
                name: "manager".to_owned(),
                description: "manager description".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent.id.clone()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![workbench.id.clone()],
            },
        })
        .await
        .expect("team_create failed");

    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    let deleted: ProjectNotifyPayload = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "workbench delete",
    )
    .await
    .parse_payload()
    .expect("parse ProjectNotify");
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == workbench.id)
    );
    let (_, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap
            .team_members
            .iter()
            .all(|member| !member.project_ids.contains(&workbench.id))
    );
}

#[tokio::test]
async fn workbench_remove_cascades_agents_terminals_sessions_and_steering() {
    let mut fixture = Fixture::new().await;

    let agent_repo = init_git_repo("agent-blocker");
    let agent_parent = create_project(&mut fixture.client, vec![agent_repo.path()]).await;
    let agent_workbench =
        create_workbench(&mut fixture.client, &agent_parent, "agent-blocker").await;
    let hold_gate = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "workbench-live-agent",
            MockScript::one(MockTurn::gated_text("hold workbench", &hold_gate)),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("workbench-live-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(agent_workbench.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&agent_workbench),
                prompt: "hold workbench".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn live workbench agent");
    let live_agent: NewAgentPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "live agent NewAgent",
    )
    .await
    .parse_payload()
    .expect("parse live agent");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::ChatEvent,
        "live agent persisted session",
    )
    .await;
    hold_gate.wait_until_entered().await;
    drop(reservation);
    let close_gate = fixture
        .host_for_test()
        .install_restore_marker_withdraw_test_gate()
        .await;
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: agent_workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    close_gate.wait_until_entered().await;
    let (mut late_observer, late_bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        late_bootstrap
            .agents
            .iter()
            .any(|agent| agent.agent_id == live_agent.agent_id)
    );
    drop(close_gate);
    // The close command cannot reach a backend parked behind this gate.
    drop(hold_gate);
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "cascaded agent close",
    )
    .await;
    let deleted: ProjectNotifyPayload = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "agent workbench delete",
    )
    .await
    .parse_payload()
    .expect("parse agent workbench delete");
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == agent_workbench.id)
    );
    let mut observed_close = false;
    next_frame_matching_on(&mut late_observer, "late subscriber sees completed workbench cleanup", |env| {
        if env.kind == FrameKind::AgentClosed {
            observed_close |= env.parse_payload::<protocol::AgentClosedPayload>().unwrap().agent_id == live_agent.agent_id;
        }
        env.kind == FrameKind::ProjectOperation && env.parse_payload::<protocol::ProjectOperationPayload>().is_ok_and(|operation| {
            matches!(operation.mutation, protocol::ProjectMutation::RemoveWorkbench(payload) if payload.id == agent_workbench.id)
                && matches!(operation.state, protocol::ProjectOperationState::Committed { .. })
        })
    }).await;
    assert!(
        observed_close,
        "a client bootstrapped during required close cleanup must receive AgentClosed before deletion completes"
    );
    eprintln!(
        "CLOSE PUBLICATION EVIDENCE: client bootstrapped during held close received AgentClosed before workbench completion"
    );
    assert!(
        !fixture.agent_ids().await.contains(&live_agent.agent_id),
        "cascaded agent remained live"
    );
    let (_, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap
            .sessions
            .iter()
            .all(|session| session.project_id.as_ref() != Some(&agent_workbench.id))
    );

    let terminal_repo = init_git_repo("terminal-blocker");
    let terminal_parent = create_project(&mut fixture.client, vec![terminal_repo.path()]).await;
    let terminal_workbench =
        create_workbench(&mut fixture.client, &terminal_parent, "terminal-blocker").await;
    fixture
        .client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: terminal_workbench.id.clone(),
                root: terminal_workbench.root_paths()[0].clone(),
                relative_cwd: None,
            },
            cols: 80,
            rows: 24,
        })
        .await
        .expect("terminal_create failed");
    let _new_terminal: NewTerminalPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewTerminal,
        "workbench terminal NewTerminal",
    )
    .await
    .parse_payload()
    .expect("parse NewTerminal");
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: terminal_workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    let deleted: ProjectNotifyPayload = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "terminal workbench delete",
    )
    .await
    .parse_payload()
    .expect("parse terminal workbench delete");
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == terminal_workbench.id)
    );

    let session_repo = init_git_repo("session-blocker");
    let session_parent = create_project(&mut fixture.client, vec![session_repo.path()]).await;
    let session_workbench =
        create_workbench(&mut fixture.client, &session_parent, "session-blocker").await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("workbench-session-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(session_workbench.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&session_workbench),
                prompt: "persist session".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn session agent");
    let new_agent: NewAgentPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "session agent NewAgent",
    )
    .await
    .parse_payload()
    .expect("parse NewAgent");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::ChatEvent,
        "session agent ChatEvent",
    )
    .await;
    fixture
        .client
        .close_agent(&new_agent.instance_stream)
        .await
        .expect("close session agent");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentClosed,
        "session agent closed",
    )
    .await;
    fixture::send_host_payload(
        &mut fixture.client,
        FrameKind::SetAgentsViewPreferences,
        &protocol::SetAgentsViewPreferencesPayload {
            update: protocol::AgentsViewPreferencesUpdate::SetSortMode {
                sort_mode: protocol::AgentSortMode::NameAsc,
            },
        },
    )
    .await;
    next_frame_matching_on(
        &mut fixture.client,
        "annotation persistence fence after close",
        |env| {
            env.kind == FrameKind::AgentsViewPreferencesNotify
                && env
                    .parse_payload::<protocol::AgentsViewPreferencesNotifyPayload>()
                    .unwrap()
                    .snapshot
                    .preferences
                    .sort_mode
                    == protocol::AgentSortMode::NameAsc
        },
    )
    .await;
    let annotation_path = fixture.store_dir().join("agents_view_preferences.json");
    let saved_annotations = fs::read(&annotation_path).unwrap();
    fs::write(&annotation_path, "invalid annotation storage").unwrap();
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: session_workbench.id.clone(),
            force: false,
        })
        .await
        .unwrap();
    let mut terminal = None;
    let mut failure = None;
    while terminal.is_none() || failure.is_none() {
        let env = fixture
            .client
            .next_event()
            .await
            .unwrap()
            .expect("required cleanup failure");
        if env.kind == FrameKind::ProjectOperation {
            let operation: protocol::ProjectOperationPayload = env.parse_payload().unwrap();
            if matches!(operation.mutation, protocol::ProjectMutation::RemoveWorkbench(payload) if payload.id == session_workbench.id)
            {
                match operation.state {
                    protocol::ProjectOperationState::Committed { .. } => {
                        panic!("corrupt required annotation storage must block catalog Delete")
                    }
                    protocol::ProjectOperationState::Failed {
                        cleanup_may_be_partial,
                        ..
                    } => {
                        assert!(cleanup_may_be_partial);
                        terminal = Some(());
                    }
                    _ => {}
                }
            }
        } else if env.kind == FrameKind::CommandError {
            failure = Some(env.parse_payload::<CommandErrorPayload>().unwrap());
        }
    }
    let (_, incomplete) = fixture.connect_with_bootstrap().await;
    assert!(
        incomplete
            .projects
            .iter()
            .any(|project| project.id == session_workbench.id)
    );
    assert!(
        incomplete
            .sessions
            .iter()
            .any(|session| session.project_id.as_ref() == Some(&session_workbench.id)),
        "dependent cleanup must not consume session identities before it can complete"
    );
    fs::write(&annotation_path, saved_annotations).unwrap();
    eprintln!(
        "PROJECT MUTATION EVIDENCE: corrupt required annotations reject deletion; catalog and session identities remain for explicit repair/retry"
    );
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: session_workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    let deleted: ProjectNotifyPayload = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "session workbench delete",
    )
    .await
    .parse_payload()
    .expect("parse session workbench delete");
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == session_workbench.id)
    );
    let (_, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap
            .sessions
            .iter()
            .all(|session| session.project_id.as_ref() != Some(&session_workbench.id))
    );

    let steering_repo = init_git_repo("steering-blocker");
    let steering_parent = create_project(&mut fixture.client, vec![steering_repo.path()]).await;
    let steering_workbench =
        create_workbench(&mut fixture.client, &steering_parent, "steering-blocker").await;
    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: Steering {
                id: SteeringId("workbench-steering".to_owned()),
                scope: SteeringScope::Project(steering_workbench.id.clone()),
                title: "Workbench steering".to_owned(),
                content: "Do not remove yet".to_owned(),
            },
        })
        .await
        .expect("steering_upsert failed");
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: steering_workbench.id.clone(),
            force: false,
        })
        .await
        .expect("workbench_remove write failed");
    let deleted: ProjectNotifyPayload = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "steering workbench delete",
    )
    .await
    .parse_payload()
    .expect("parse steering workbench delete");
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == steering_workbench.id)
    );
    let (_, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    assert!(
        bootstrap
            .steering
            .iter()
            .all(|steering| !matches!(&steering.scope, SteeringScope::Project(project_id) if project_id == &steering_workbench.id))
    );
}

#[tokio::test]
async fn agent_control_creates_lists_and_spawns_workbenches() {
    let repo = init_git_repo("agent-control-workbench");
    let mut fixture = Fixture::new().await;
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("workbench-orchestrator".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(parent.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&parent),
                prompt: "orchestrate workbenches".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await
        .expect("spawn orchestrator");
    let mut orchestrator = None;
    next_frame_matching_on(&mut fixture.client, "orchestrator", |env| {
        if env.kind != FrameKind::NewAgent {
            return false;
        }
        let agent: NewAgentPayload = env.parse_payload().expect("NewAgent");
        if agent.name == "workbench-orchestrator" {
            orchestrator = Some(agent);
            true
        } else {
            false
        }
    })
    .await;
    let orchestrator = orchestrator.expect("orchestrator NewAgent");

    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_list_workbenches",
        json!({}),
    )
    .await;
    assert!(!is_error, "list failed: {body}");
    let listed: Value = serde_json::from_str(&body).expect("list JSON");
    assert_eq!(listed["caller_project_id"], parent.id.0);
    assert_eq!(listed["projects"].as_array().expect("projects").len(), 1);

    let first_commit = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("second.txt"), "second commit").expect("second content");
    git(repo.path(), &["add", "second.txt"]);
    git(repo.path(), &["commit", "-m", "second"]);
    let base_head = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("dirty.txt"), "not copied").expect("dirty parent");
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_create_workbench",
        json!({
            "parent_project_id": parent.id.0, "branch": "feature/mcp-default"
        }),
    )
    .await;
    assert!(!is_error, "create failed: {body}");
    let created: Value = serde_json::from_str(&body).expect("create JSON");
    let workbench_id = created["project_id"]
        .as_str()
        .expect("project id")
        .to_owned();
    let root = &created["roots"][0];
    assert_eq!(created["entry_scheduled"], false);
    assert_eq!(root["base_commit"], base_head);
    assert_eq!(root["parent_root_dirty"], true);
    let worktree_root = root["worktree_root"]
        .as_str()
        .expect("worktree root")
        .to_owned();
    assert!(!Path::new(&worktree_root).join("dirty.txt").exists());
    let ProjectNotifyPayload::Upsert { project: workbench } =
        expect_project_notify(&mut fixture.client, "MCP workbench upsert").await
    else {
        panic!("expected workbench upsert")
    };
    let ProjectSource::GitWorkbench {
        parent_project_id,
        branch,
        roots,
        ..
    } = &workbench.source
    else {
        panic!("expected GitWorkbench source")
    };
    assert_eq!(parent_project_id, &parent.id);
    assert_eq!(branch.0, "feature/mcp-default");
    assert_eq!(roots[0].parent_root.0, repo.path().display().to_string());
    assert_eq!(roots[0].worktree_root.0, worktree_root);

    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_spawn_agent",
        json!({
            "project_id": workbench_id, "prompt": "work in isolation", "backend_kind": "codex"
        }),
    )
    .await;
    assert!(!is_error, "derived-root spawn failed: {body}");
    let spawned: NewAgentPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "derived-root NewAgent",
    )
    .await
    .parse_payload()
    .expect("NewAgent payload");
    assert_eq!(spawned.project_id.as_ref(), Some(&workbench.id));
    assert_eq!(
        spawned.parent_agent_id.as_ref(),
        Some(&orchestrator.agent_id)
    );
    assert_eq!(spawned.workspace_roots, vec![worktree_root.clone()]);
    let started: AgentStartPayload = expect_kind(
        &mut fixture.client,
        FrameKind::AgentStart,
        "derived-root AgentStart",
    )
    .await
    .parse_payload()
    .expect("AgentStart payload");
    assert_eq!(started.project_id.as_ref(), Some(&workbench.id));
    assert_eq!(
        started.parent_agent_id.as_ref(),
        Some(&orchestrator.agent_id)
    );
    assert_eq!(started.workspace_roots, vec![worktree_root.clone()]);

    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_spawn_agent",
        json!({
            "project_id": workbench_id, "workspace_roots": [repo.path()],
            "prompt": "escape", "backend_kind": "codex"
        }),
    )
    .await;
    assert!(is_error, "mismatched roots unexpectedly spawned: {body}");
    assert!(body.contains("authoritative roots"));
    assert_no_new_agent(&mut fixture.client, "rejecting mismatched roots").await;

    // global=true lets the agent discover another project and start an
    // independent top-level agent there instead of a child in its own project.
    let other_repo = init_git_repo("agent-control-other-project");
    let other = create_project(&mut fixture.client, vec![other_repo.path()]).await;
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_list_workbenches",
        json!({}),
    )
    .await;
    assert!(!is_error, "scoped list failed: {body}");
    let scoped: Value = serde_json::from_str(&body).expect("scoped list JSON");
    assert!(
        scoped["projects"]
            .as_array()
            .expect("scoped projects")
            .iter()
            .all(|project| project["project_id"] != other.id.0),
        "default listing must stay within the caller's project"
    );
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_list_workbenches",
        json!({ "global": true }),
    )
    .await;
    assert!(!is_error, "global list failed: {body}");
    let global: Value = serde_json::from_str(&body).expect("global list JSON");
    assert_eq!(global["caller_project_id"], parent.id.0);
    let global_ids = global["projects"]
        .as_array()
        .expect("global projects")
        .iter()
        .filter_map(|project| project["project_id"].as_str())
        .collect::<Vec<_>>();
    for expected in [&parent.id, &workbench.id, &other.id] {
        assert!(
            global_ids.contains(&expected.0.as_str()),
            "global listing must include every host project"
        );
    }
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_spawn_agent",
        json!({
            "project_id": other.id.0, "prompt": "work elsewhere",
            "backend_kind": "codex", "global": true
        }),
    )
    .await;
    assert!(!is_error, "global spawn failed: {body}");
    let top_level: NewAgentPayload =
        expect_kind(&mut fixture.client, FrameKind::NewAgent, "global NewAgent")
            .await
            .parse_payload()
            .expect("global NewAgent payload");
    assert_eq!(top_level.project_id.as_ref(), Some(&other.id));
    assert_eq!(top_level.parent_agent_id, None);
    assert_eq!(top_level.workspace_roots, project_roots(&other));
    let top_level_start: AgentStartPayload = expect_kind(
        &mut fixture.client,
        FrameKind::AgentStart,
        "global AgentStart",
    )
    .await
    .parse_payload()
    .expect("global AgentStart payload");
    assert_eq!(top_level_start.project_id.as_ref(), Some(&other.id));
    assert_eq!(top_level_start.parent_agent_id, None);
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_spawn_agent",
        json!({"prompt": "inherit parent project", "backend_kind": "codex"}),
    )
    .await;
    assert!(!is_error, "parent-project inheritance failed: {body}");
    let inherited_spawn: NewAgentPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "inherited-project NewAgent",
    )
    .await
    .parse_payload()
    .expect("inherited-project payload");
    assert_eq!(inherited_spawn.project_id.as_ref(), Some(&parent.id));
    assert_eq!(
        inherited_spawn.parent_agent_id.as_ref(),
        Some(&orchestrator.agent_id)
    );
    assert_eq!(inherited_spawn.workspace_roots, project_roots(&parent));
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentStart,
        "inherited-project AgentStart",
    )
    .await;

    let projectless_parent_root = tempfile::tempdir().expect("projectless parent root");
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("projectless-parent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![projectless_parent_root.path().display().to_string()],
                prompt: "projectless parent".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await
        .expect("spawn projectless parent");
    let projectless_parent: NewAgentPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "projectless parent NewAgent",
    )
    .await
    .parse_payload()
    .expect("projectless parent payload");
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentStart,
        "projectless parent AgentStart",
    )
    .await;
    let (is_error, body) = call_agent_control(
        &fixture,
        &projectless_parent.agent_id,
        "tyde_spawn_agent",
        json!({"prompt": "missing roots", "backend_kind": "codex"}),
    )
    .await;
    assert!(
        is_error,
        "projectless rootless spawn unexpectedly succeeded: {body}"
    );
    assert_no_new_agent(&mut fixture.client, "rejecting projectless rootless spawn").await;

    let explicit_root = tempfile::tempdir().expect("explicit projectless root");
    let explicit_root_path = explicit_root.path().display().to_string();
    let (is_error, body) = call_agent_control(
        &fixture,
        &projectless_parent.agent_id,
        "tyde_spawn_agent",
        json!({
            "workspace_roots": [explicit_root_path],
            "prompt": "valid projectless explicit root",
            "backend_kind": "codex"
        }),
    )
    .await;
    assert!(!is_error, "projectless explicit-root spawn failed: {body}");
    let explicit_spawn: NewAgentPayload = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "projectless explicit-root NewAgent",
    )
    .await
    .parse_payload()
    .expect("projectless explicit-root payload");
    assert_eq!(explicit_spawn.project_id, None);
    assert_eq!(
        explicit_spawn.parent_agent_id.as_ref(),
        Some(&projectless_parent.agent_id)
    );
    assert_eq!(explicit_spawn.workspace_roots, vec![explicit_root_path]);
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::AgentStart,
        "projectless explicit-root AgentStart",
    )
    .await;

    for arguments in [
        json!({"parent_project_id": parent.id.0, "branch": "feature/invalid", "base_ref": "--help"}),
        json!({"parent_project_id": parent.id.0, "branch": "feature/blank-name", "name": "   "}),
    ] {
        let (is_error, body) = call_agent_control(
            &fixture,
            &orchestrator.agent_id,
            "tyde_create_workbench",
            arguments,
        )
        .await;
        assert!(is_error, "invalid create unexpectedly succeeded: {body}");
    }
    let (is_error, body) = call_agent_control(&fixture, &orchestrator.agent_id,
        "tyde_create_workbench", json!({
            "parent_project_id": parent.id.0, "branch": "feature/historical", "base_ref": first_commit
        })).await;
    assert!(!is_error, "historical create failed: {body}");
    let historical: Value = serde_json::from_str(&body).expect("historical JSON");
    assert_eq!(historical["roots"][0]["base_commit"], first_commit);
    let historical_root = historical["roots"][0]["worktree_root"]
        .as_str()
        .expect("root");
    assert_eq!(
        git_stdout(Path::new(historical_root), &["rev-parse", "HEAD"]),
        first_commit
    );
    assert!(!Path::new(historical_root).join("second.txt").exists());
    let _ = expect_project_notify(&mut fixture.client, "historical upsert").await;

    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_list_workbenches",
        json!({}),
    )
    .await;
    assert!(!is_error, "recovery list failed: {body}");
    let recovery: Value = serde_json::from_str(&body).expect("recovery list JSON");
    assert_eq!(recovery["projects"].as_array().expect("projects").len(), 3);
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_create_workbench",
        json!({
            "parent_project_id": parent.id.0, "branch": "feature/mcp-default"
        }),
    )
    .await;
    assert!(is_error, "duplicate create unexpectedly succeeded: {body}");
    let (is_error, body) = call_agent_control(
        &fixture,
        &orchestrator.agent_id,
        "tyde_list_workbenches",
        json!({}),
    )
    .await;
    assert!(!is_error, "post-conflict list failed: {body}");
    let post_conflict: Value = serde_json::from_str(&body).expect("post-conflict JSON");
    assert_eq!(
        post_conflict["projects"]
            .as_array()
            .expect("projects")
            .iter()
            .filter(|project| project["branch"] == "feature/mcp-default")
            .count(),
        1
    );
    fixture
        .host_for_test()
        .stop_workflow_watcher_for_test()
        .await
        .unwrap();
    let (is_error, body) = call_agent_control(&fixture, &orchestrator.agent_id, "tyde_create_workbench", json!({"parent_project_id": parent.id.0, "branch": "workflow-publication-error", "action": "create_only"})).await;
    assert!(
        is_error && body.contains("Catalog committed"),
        "MCP must distinguish a durable commit from failed derived work"
    );
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    let committed = bootstrap.projects.into_iter().find(|project| matches!(&project.source, ProjectSource::GitWorkbench { branch, .. } if branch.0 == "workflow-publication-error")).expect("durably committed workbench must remain authoritative");
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: committed.id.clone(),
            force: false,
        })
        .await
        .unwrap();
    let mut deleted = false;
    let mut warning = false;
    let mut reported = false;
    while !deleted || !warning || !reported {
        let env = fixture
            .client
            .next_event()
            .await
            .unwrap()
            .expect("post-commit deletion outcome");
        match env.kind {
            FrameKind::ProjectNotify => {
                deleted |= matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(), ProjectNotifyPayload::Delete { project } if project.id == committed.id);
            }
            FrameKind::ProjectOperation => {
                let operation: protocol::ProjectOperationPayload = env.parse_payload().unwrap();
                if matches!(operation.mutation, protocol::ProjectMutation::RemoveWorkbench(payload) if payload.id == committed.id)
                {
                    match operation.state {
                        protocol::ProjectOperationState::Committed { warnings } => {
                            assert!(warnings.iter().any(|warning| warning.stage
                                == protocol::ProjectOperationStage::Workflow));
                            warning = true;
                        }
                        protocol::ProjectOperationState::Failed { .. } => panic!(
                            "derived failure must not turn durable deletion into failed mutation"
                        ),
                        _ => {}
                    }
                }
            }
            FrameKind::CommandError => {
                let error: CommandErrorPayload = env.parse_payload().unwrap();
                reported |= error.message.contains("Catalog committed")
                    && matches!(error.context, Some(protocol::CommandErrorContext::ProjectMutationCommitted { mutation: protocol::ProjectMutation::RemoveWorkbench(payload), warnings }) if payload.id == committed.id && warnings.iter().any(|warning| warning.stage == protocol::ProjectOperationStage::Workflow));
            }
            _ => {}
        }
    }
    assert!(!expected_worktree_path(repo.path(), "workflow-publication-error").exists());
    eprintln!(
        "PROJECT MUTATION EVIDENCE: stopped real workflow owner reports explicit post-commit MCP/connection errors; catalog create/delete remain authoritative"
    );
}

#[tokio::test]
async fn agent_control_workbenches_enforce_auth_access_and_scope() {
    let repo = init_git_repo("scope-parent");
    let other_repo = init_git_repo("scope-other");
    let mut fixture =
        Fixture::new_with_mock_backend_for_enabled_backends(vec![BackendKind::Codex]).await;
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let other = create_project(&mut fixture.client, vec![other_repo.path()]).await;
    let removable = create_workbench(&mut fixture.client, &parent, "feature/mcp-remove").await;
    let readonly_removable =
        create_workbench(&mut fixture.client, &parent, "feature/read-only-remove").await;
    let other_workbench =
        create_workbench(&mut fixture.client, &other, "feature/out-of-scope-remove").await;
    let removable_root = PathBuf::from(project_roots(&removable)[0].clone());
    let readonly_removable_root = PathBuf::from(project_roots(&readonly_removable)[0].clone());
    fs::write(removable_root.join("uncommitted.txt"), "discard me\n")
        .expect("write dirty MCP workbench file");

    for (tool, arguments) in [
        ("tyde_move_to_project", json!({"project_id": other.id.0})),
        ("tyde_list_workbenches", json!({})),
        (
            "tyde_create_workbench",
            json!({
                "parent_project_id": parent.id.0, "branch": "feature/no-auth"
            }),
        ),
        (
            "tyde_remove_workbench",
            json!({"project_id": removable.id.0}),
        ),
    ] {
        let (is_error, body) = call_agent_control_optional(&fixture, None, tool, arguments).await;
        assert!(is_error, "unauthenticated {tool} succeeded: {body}");
    }
    assert!(!expected_worktree_path(repo.path(), "feature-no-auth").exists());

    let read_only = spawn_project_caller(
        &mut fixture.client,
        &parent,
        "read-only-workbench-caller",
        BackendAccessMode::ReadOnly,
    )
    .await;
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &read_only.instance_stream,
        "read-only caller idle",
        |event| matches!(event, protocol::ChatEvent::TypingStatusChanged(false)),
    )
    .await;
    let (is_error, body) = call_agent_control(
        &fixture,
        &read_only.agent_id,
        "tyde_list_workbenches",
        json!({}),
    )
    .await;
    assert!(!is_error, "read-only list failed: {body}");
    let (is_error, body) = call_agent_control(
        &fixture,
        &read_only.agent_id,
        "tyde_create_workbench",
        json!({"parent_project_id": parent.id.0, "branch": "feature/read-only"}),
    )
    .await;
    assert!(!is_error, "read-only guidance mode create failed: {body}");
    assert!(expected_worktree_path(repo.path(), "feature-read-only").exists());
    let (is_error, body) = call_agent_control(
        &fixture,
        &read_only.agent_id,
        "tyde_remove_workbench",
        json!({"project_id": readonly_removable.id.0}),
    )
    .await;
    assert!(!is_error, "read-only guidance mode remove failed: {body}");
    assert!(
        !readonly_removable_root.exists(),
        "workbench was not removed"
    );

    let caller = spawn_project_caller(
        &mut fixture.client,
        &parent,
        "scoped-workbench-caller",
        BackendAccessMode::Unrestricted,
    )
    .await;
    let (is_error, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_create_workbench",
        json!({"parent_project_id": other.id.0, "branch": "feature/out-of-scope"}),
    )
    .await;
    assert!(is_error, "out-of-scope create succeeded: {body}");
    assert!(body.contains("outside caller project scope"));
    assert!(!expected_worktree_path(other_repo.path(), "feature-out-of-scope").exists());
    let (is_error, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_remove_workbench",
        json!({"project_id": other_workbench.id.0}),
    )
    .await;
    assert!(is_error, "out-of-scope remove succeeded: {body}");
    assert!(body.contains("outside caller project scope"));

    let self_caller = spawn_project_caller(
        &mut fixture.client,
        &removable,
        "self-removing-workbench-caller",
        BackendAccessMode::Unrestricted,
    )
    .await;
    let (is_error, body) = call_agent_control(
        &fixture,
        &self_caller.agent_id,
        "tyde_remove_workbench",
        json!({"project_id": removable.id.0}),
    )
    .await;
    assert!(is_error, "self workbench remove succeeded: {body}");
    assert!(body.contains("caller's active workbench"));

    let (is_error, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_remove_workbench",
        json!({"project_id": removable.id.0}),
    )
    .await;
    assert!(is_error, "safe dirty remove succeeded: {body}");
    assert!(body.contains("?? uncommitted.txt"));
    assert!(removable_root.join("uncommitted.txt").exists());

    let (is_error, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_remove_workbench",
        json!({"project_id": removable.id.0, "force": true}),
    )
    .await;
    assert!(!is_error, "scoped remove failed: {body}");
    let removed: Value = serde_json::from_str(&body).expect("remove JSON");
    assert_eq!(removed["project_id"], removable.id.0);
    assert_eq!(removed["removed"], true);
    let deleted: ProjectNotifyPayload = expect_kind(
        &mut fixture.client,
        FrameKind::ProjectNotify,
        "MCP workbench delete",
    )
    .await
    .parse_payload()
    .expect("parse MCP workbench delete");
    assert!(
        matches!(deleted, ProjectNotifyPayload::Delete { project } if project.id == removable.id)
    );

    let busy = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Moving child",
            MockScript::one(MockTurn::gated_text("Child working before move", &busy)),
        )
        .await;
    let (failed, _) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_spawn_agent",
        json!({
            "name": "Moving child", "prompt": "Work for the parent", "backend_kind": "codex"
        }),
    )
    .await;
    assert!(!failed, "spawn controlled child");
    let child = expect_kind(&mut fixture.client, FrameKind::NewAgent, "moving child")
        .await
        .parse_payload::<NewAgentPayload>()
        .unwrap();
    let child_start = fixture::next_logical_frame_matching_on(
        &mut fixture.client,
        "moving child registered session",
        |env| {
            env.stream == child.instance_stream
                && env.kind == FrameKind::AgentStart
                && env
                    .parse_payload::<AgentStartPayload>()
                    .unwrap()
                    .session_id
                    .is_some()
        },
    )
    .await
    .parse_payload::<AgentStartPayload>()
    .unwrap();
    assert_eq!(child_start.agent_id, child.agent_id);
    drop(reservation);
    busy.wait_until_entered().await;
    let (failed, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_move_to_project",
        json!({
            "agent_id": child.agent_id.0, "project_id": other_workbench.id.0
        }),
    )
    .await;
    assert!(
        failed && body.contains("finished"),
        "controlled move must preserve the idle boundary"
    );
    busy.release_one();
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &child.instance_stream,
        "child idle",
        |event| matches!(event, protocol::ChatEvent::TypingStatusChanged(false)),
    )
    .await;
    for (moving_caller, target) in [
        (None, &child.agent_id),
        (Some(&read_only.agent_id), &child.agent_id),
        (Some(&child.agent_id), &caller.agent_id),
        (Some(&caller.agent_id), &read_only.agent_id),
    ] {
        let (failed, _) = call_agent_control_optional(
            &fixture,
            moving_caller,
            "tyde_move_to_project",
            json!({
                "agent_id": target.0, "project_id": other_workbench.id.0
            }),
        )
        .await;
        assert!(
            failed,
            "move must authenticate and retain control ownership, even for top-level ordinary Agents"
        );
    }
    let (failed, _) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_move_to_project",
        json!({
            "agent_id": child.agent_id.0, "project_id": "missing-project"
        }),
    )
    .await;
    assert!(
        failed,
        "authorized control cannot create a missing destination"
    );
    let (_, before) = fixture.connect_with_bootstrap().await;
    let before = before
        .agents
        .iter()
        .find(|entry| entry.agent_id == child.agent_id)
        .unwrap();
    assert_eq!(
        before.project_id,
        Some(parent.id.clone()),
        "denied moves must not mutate assignment"
    );
    assert_eq!(
        before.tyde_permissions,
        protocol::AgentTydePermissions::Agent
    );
    let mut observer = fixture.connect().await;
    for destination in [Some(&other), Some(&other_workbench), None] {
        let project_id = destination.map(|project| project.id.clone());
        let roots = destination.map_or_else(|| project_roots(&other_workbench), project_roots);
        let (failed, body) = call_agent_control(
            &fixture,
            &caller.agent_id,
            "tyde_move_to_project",
            json!({
                "agent_id": child.agent_id.0, "project_id": project_id
            }),
        )
        .await;
        eprintln!(
            "CROSS-PROJECT CHILD MOVE: destination_assigned={} succeeded={}",
            project_id.is_some(),
            !failed
        );
        assert!(
            !failed,
            "an owned child must be movable across project boundaries"
        );
        let result: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            result["entry_scheduled"], false,
            "controlled idle moves must not create an automatic turn"
        );
        let moved = expect_kind(
            &mut fixture.client,
            FrameKind::AgentProjectChanged,
            "controlled child move",
        )
        .await
        .parse_payload::<AgentStartPayload>()
        .unwrap();
        assert_eq!(moved.agent_id, child.agent_id);
        assert_eq!(moved.project_id, project_id);
        assert_eq!(moved.workspace_roots, roots);
        assert!(
            moved.session_id == child_start.session_id,
            "child conversation is preserved"
        );
        assert_eq!(moved.parent_agent_id, Some(caller.agent_id.clone()));
        assert_eq!(
            moved.tyde_permissions,
            protocol::AgentTydePermissions::Agent
        );
        let broadcast = expect_kind(
            &mut observer,
            FrameKind::AgentProjectChanged,
            "controlled child move observer",
        )
        .await
        .parse_payload::<AgentStartPayload>()
        .unwrap();
        assert_eq!(broadcast.project_id, project_id);
        assert_eq!(broadcast.workspace_roots, roots);
        fixture
            .mock_by_id(&child.agent_id)
            .await
            .enqueue(MockTurn::workspace_roots())
            .await;
        let (failed, _) = call_agent_control(
            &fixture,
            &caller.agent_id,
            "tyde_send_agent_message",
            json!({
                "agent_id": child.agent_id.0, "message": "Report the effective workspace"
            }),
        )
        .await;
        assert!(
            !failed,
            "cross-project child remains controllable by its parent"
        );
        let turn = fixture::finish_turn_on(&mut fixture.client, &child.instance_stream).await;
        let reported = turn
            .chat_events()
            .into_iter()
            .find_map(|event| match event {
                protocol::ChatEvent::StreamEnd(end) => Some(end.message.content),
                _ => None,
            })
            .expect("child workspace report");
        let reported: Vec<String> = serde_json::from_str(&reported).unwrap();
        assert_eq!(
            reported, roots,
            "controlled child execution must use destination roots"
        );
        let (_, state) = fixture.connect_with_bootstrap().await;
        let saved = state
            .sessions
            .iter()
            .find(|saved| Some(&saved.id) == child_start.session_id.as_ref())
            .unwrap();
        assert_eq!(saved.project_id, project_id);
        assert_eq!(saved.workspace_roots, roots);
        let parent_state = state
            .agents
            .iter()
            .find(|entry| entry.agent_id == caller.agent_id)
            .unwrap();
        assert_eq!(parent_state.project_id, Some(parent.id.clone()));
        assert_eq!(
            parent_state.tyde_permissions,
            protocol::AgentTydePermissions::Agent
        );
    }
    let caller_stream = caller.instance_stream.clone();
    for (role, destination) in [
        (protocol::AgentTydePermissions::Orchestrator, &other),
        (protocol::AgentTydePermissions::Root, &parent),
    ] {
        fixture
            .client
            .set_agent_tyde_permissions(&caller_stream, role)
            .await
            .unwrap();
        expect_kind(
            &mut fixture.client,
            FrameKind::AgentTydePermissionsChanged,
            "human-assigned move authority",
        )
        .await;
        let (failed, _) = call_agent_control(
            &fixture,
            &caller.agent_id,
            "tyde_move_to_project",
            json!({
                "agent_id": read_only.agent_id.0, "project_id": destination.id.0
            }),
        )
        .await;
        assert!(
            !failed,
            "explicit higher role must retain existing host-wide agent scope"
        );
        let moved = expect_kind(
            &mut fixture.client,
            FrameKind::AgentProjectChanged,
            "higher role move",
        )
        .await
        .parse_payload::<AgentStartPayload>()
        .unwrap();
        assert_eq!(moved.project_id, Some(destination.id.clone()));
        assert_eq!(
            moved.tyde_permissions,
            protocol::AgentTydePermissions::Agent
        );
    }
    fixture
        .client
        .set_agent_tyde_permissions(&caller_stream, protocol::AgentTydePermissions::Agent)
        .await
        .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::AgentTydePermissionsChanged,
        "restored ordinary role",
    )
    .await;
    let (failed, _) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_move_to_project",
        json!({
            "agent_id": read_only.agent_id.0, "project_id": other.id.0
        }),
    )
    .await;
    assert!(
        failed,
        "a move must not retain authority after human role revocation"
    );
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: other_workbench.id.clone(),
            force: false,
        })
        .await
        .unwrap();
    next_frame_matching_on(
        &mut fixture.client,
        "Home agent stops before workspace removal",
        |env| {
            env.kind == FrameKind::AgentClosed
                && env
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .unwrap()
                    .agent_id
                    == child.agent_id
        },
    )
    .await;
    let (_, state) = fixture.connect_with_bootstrap().await;
    assert!(
        !state
            .agents
            .iter()
            .any(|entry| entry.agent_id == child.agent_id)
    );
    assert!(
        state
            .agents
            .iter()
            .any(|entry| entry.agent_id == caller.agent_id),
        "removing the child's workspace must not close its parent in another project"
    );
}

#[tokio::test]
async fn agent_control_multi_root_preflight_and_removed_spawn_are_atomic() {
    let first = init_git_repo("multi-first");
    let second = init_git_repo("multi-second");
    git(first.path(), &["branch", "first-only-base"]);
    let mut fixture = Fixture::new().await;
    let parent = create_project(&mut fixture.client, vec![first.path(), second.path()]).await;
    let caller = spawn_project_caller(
        &mut fixture.client,
        &parent,
        "multi-root-caller",
        BackendAccessMode::Unrestricted,
    )
    .await;
    let (is_error, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_create_workbench",
        json!({
            "parent_project_id": parent.id.0,
            "branch": "feature/multi-fail",
            "base_ref": "first-only-base"
        }),
    )
    .await;
    assert!(
        is_error,
        "multi-root preflight unexpectedly succeeded: {body}"
    );
    assert!(body.contains(&second.path().display().to_string()));
    for root in [first.path(), second.path()] {
        assert!(!expected_worktree_path(root, "feature-multi-fail").exists());
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/feature/multi-fail",
            ])
            .status()
            .expect("git show-ref");
        assert!(!output.success());
    }
    assert_no_project_upsert(&mut fixture.client, "rejecting multi-root preflight").await;

    let removable = create_workbench(&mut fixture.client, &parent, "feature/removed").await;
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: removable.id.clone(),
            force: false,
        })
        .await
        .expect("remove workbench");
    let ProjectNotifyPayload::Delete { project } =
        expect_project_notify(&mut fixture.client, "removed workbench delete").await
    else {
        panic!("expected workbench delete")
    };
    assert_eq!(project.id, removable.id);
    let (is_error, body) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_spawn_agent",
        json!({
            "project_id": removable.id.0,
            "prompt": "must not register",
            "backend_kind": "codex"
        }),
    )
    .await;
    assert!(is_error, "spawn into removed workbench succeeded: {body}");
    assert!(body.contains("missing project"));
    assert_no_new_agent(&mut fixture.client, "rejecting removed-workbench spawn").await;
}

#[tokio::test]
async fn concurrent_workbench_remove_and_mcp_spawn_have_one_winner() {
    let repo = init_git_repo("remove-spawn-race");
    let mut fixture = Fixture::new().await;
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let caller = spawn_project_caller(
        &mut fixture.client,
        &parent,
        "remove-spawn-race-caller",
        BackendAccessMode::Unrestricted,
    )
    .await;
    let workbench = create_workbench(&mut fixture.client, &parent, "feature/remove-spawn").await;
    let hook = fixture.install_workbench_remove_test_hook();
    let (mut remove_client, _) = fixture.connect_with_bootstrap().await;
    remove_client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
            force: false,
        })
        .await
        .expect("send concurrent remove");
    (hook.wait_until_reached()).await;

    let caller_auth = fixture.agent_control_caller(&caller.agent_id).await;
    let bearer = caller_auth
        .authorization
        .strip_prefix("Bearer ")
        .expect("bearer credential")
        .to_owned();
    let url = fixture.agent_control_http_url().await;
    let project_id = workbench.id.0.clone();
    let spawn_task = tokio::spawn(async move {
        call_agent_control_request(
            url,
            Some(bearer),
            "tyde_spawn_agent".to_owned(),
            json!({
                "project_id": project_id,
                "prompt": "race removal",
                "backend_kind": "codex"
            }),
        )
        .await
    });
    hook.wait_until_spawn_waiting().await;
    let (is_error, body) = spawn_task.await.expect("spawn task join");
    assert!(is_error, "spawn must reject while removal is still held");
    assert!(
        body.contains("being removed"),
        "admission must report the held removal conflict"
    );
    hook.resume();

    let ProjectNotifyPayload::Delete { project } =
        expect_project_notify(&mut fixture.client, "concurrent remove winner").await
    else {
        panic!("removal did not emit Delete after winning the shared lock")
    };
    assert_eq!(project.id, workbench.id);
    assert_no_new_agent(
        &mut fixture.client,
        "verifying losing concurrent spawn did not register",
    )
    .await;
}

#[tokio::test]
async fn agent_enters_its_workbench_at_turn_boundary_and_reviews_that_project() {
    let repo = init_git_repo("self-entry");
    let extra = init_git_repo("self-entry-extra");
    let unrelated_repo = init_git_repo("unrelated-entry");
    let mut fixture =
        Fixture::new_with_mock_backend_for_enabled_backends(vec![BackendKind::Codex]).await;
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    let parent = create_project(&mut fixture.client, vec![repo.path(), extra.path()]).await;
    let unrelated = create_project(&mut fixture.client, vec![unrelated_repo.path()]).await;
    let config_url = fixture.config_mcp_http_url().await;
    for (name, arguments) in [
        (
            "tyde_config_upsert_review_aspect",
            json!({"aspect": {
                "name": "Workspace", "description": "Workspace ownership", "instructions": "Review the frozen changes", "enabled": true
            }}),
        ),
        (
            "tyde_config_set_setting",
            json!({"setting": {"setting": "review_team", "team": {"layout": "per_reviewer", "reviewers": [
                {"id": "entry-reviewer", "name": "Entry", "target": {"kind": "explicit", "backend_kind": "codex", "session_settings": {}}}
            ]}}}),
        ),
    ] {
        let (failed, _) =
            call_agent_control_request(config_url.clone(), None, name.to_owned(), arguments).await;
        assert!(!failed, "entry review configuration failed");
    }
    let controller_reservation = fixture
        .reserve_next_mock_launch("Entry controller", MockScript::one(MockTurn::text("Ready")))
        .await;
    let (controller, controller_start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Entry controller".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(parent.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&parent),
                prompt: "Control the diagnostic flow".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await;
    drop(controller_reservation);
    fixture.finish_turn(&controller).await;
    let initial = MockGateHandle::new();
    let continued = MockGateHandle::new();
    let shutdown = MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Entering worker",
            MockScript::one(MockTurn::held_text("Active turn before MCP cancellation"))
                .then(MockTurn::held_text(
                    "Active turn before configuration cancellation",
                ))
                .then(MockTurn::async_question_request(
                    "entry-unanswered-question",
                    &initial,
                ))
                .with_mid_turn_steering()
                .with_shutdown_gate(&shutdown)
                .with_user_bubbles(),
        )
        .await;
    let (agent, start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Entering worker".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(parent.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&parent),
                prompt: "Work in an isolated workbench".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await;
    drop(reservation);
    fixture
        .next_chat_event_matching(&agent, "first held turn active", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(true))
        })
        .await;
    let auth = fixture
        .agent_control_caller(&controller_start.agent_id)
        .await;
    let bearer = auth
        .authorization
        .strip_prefix("Bearer ")
        .expect("controller credential")
        .to_owned();
    for (url, name, global) in [
        (
            fixture.agent_control_http_url().await,
            "tyde_send_agent_message",
            true,
        ),
        (config_url.clone(), "tyde_config_send_agent_message", false),
    ] {
        let mut arguments = json!({"agent_id": start.agent_id, "message": "Continue after cancellation", "interrupt": true});
        if global {
            arguments["global"] = json!(true);
        }
        let (failed, _) =
            call_agent_control_request(url, Some(bearer.clone()), name.to_owned(), arguments).await;
        assert!(!failed, "active MCP delivery must be accepted");
        fixture
            .next_chat_event_matching(&agent, "interrupted held turn idle", |event| {
                matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
            })
            .await;
        fixture
            .next_chat_event_matching(&agent, "replacement turn active", |event| {
                matches!(event, protocol::ChatEvent::TypingStatusChanged(true))
            })
            .await;
    }
    initial.wait_until_entered().await;
    fixture
        .next_chat_event_matching(
            &agent,
            "nonblocking question registered before entry",
            |event| {
                matches!(event, protocol::ChatEvent::ToolRequest(request)
                if matches!(request.tool_type, protocol::ToolRequestType::AskUserQuestion {
                    mode: protocol::UserQuestionMode::NonBlocking, ..
                }))
            },
        )
        .await;
    let diagnostic_mock = fixture.mock(&agent).await;
    let active_requests = diagnostic_mock.requests().await;
    assert_eq!(
        active_requests
            .iter()
            .filter(|request| matches!(request, server::backend::mock::MockRequest::Interrupt))
            .count(),
        2,
        "each active interrupt surface must reach Backend::interrupt"
    );
    let (failed, _) = call_agent_control_request(
        fixture.agent_control_http_url().await,
        Some(bearer.clone()),
        "tyde_send_agent_message".to_owned(),
        json!({"agent_id": start.agent_id, "message": "Steer the current turn", "global": true}),
    )
    .await;
    assert!(!failed, "native steer admitted");
    assert_eq!(
        diagnostic_mock
            .requests()
            .await
            .iter()
            .filter(|request| matches!(request, server::backend::mock::MockRequest::Steer(_)))
            .count(),
        1
    );
    eprintln!("ENTRY DIAGNOSTIC active surfaces: backend_interrupts=2 native_steers=1");
    diagnostic_mock
        .enqueue_all([
            MockTurn::gated_text("Working inside the workbench", &continued),
            MockTurn::workspace_roots(),
            MockTurn::workspace_roots(),
        ])
        .await;
    let mut observer = fixture.connect().await;
    let (failed, created) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_create_workbench",
        json!({
            "parent_project_id": parent.id.0, "branch": "feature/self-entry", "action": "enter"
        }),
    )
    .await;
    assert!(!failed, "create and enter failed");
    let created: Value = serde_json::from_str(&created).expect("entry JSON");
    assert_eq!(created["entry_scheduled"], true);
    let workbench_id = ProjectId(created["project_id"].as_str().unwrap().to_owned());
    let roots: Vec<String> = created["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|root| root["worktree_root"].as_str().unwrap().to_owned())
        .collect();
    let (_, before) = fixture.connect_with_bootstrap().await;
    let before = before
        .agents
        .iter()
        .find(|entry| entry.agent_id == start.agent_id)
        .unwrap();
    assert_eq!(
        before.project_id,
        Some(parent.id.clone()),
        "pending entry must not relabel a running turn"
    );
    assert_eq!(before.workspace_roots, project_roots(&parent));
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_review",
        json!({"request_id": "workbench-review"}),
    )
    .await;
    assert!(failed, "pending entry must not review the old project");
    fixture
        .client
        .send_message(&agent.stream, "Ordinary queued work".to_owned())
        .await
        .unwrap();
    let ordinary = fixture.expect_queued_messages(&agent, 1).await;
    initial.release_one();
    fixture
        .next_chat_event_matching(
            &agent,
            "idle with unanswered nonblocking question",
            |event| matches!(event, protocol::ChatEvent::TypingStatusChanged(false)),
        )
        .await;
    // Queueing and move admission alone must not cancel the unanswered card.
    fixture.mock(&agent).await;
    let held_requests = diagnostic_mock.requests_after_commands().await;
    assert_eq!(
        held_requests
            .iter()
            .filter(|request| matches!(request, server::backend::mock::MockRequest::Interrupt))
            .count(),
        2
    );
    assert_eq!(
        held_requests
            .iter()
            .filter(|request| matches!(request, server::backend::mock::MockRequest::Input(_)))
            .count(),
        2
    );
    let (failed, _) = call_agent_control_request(
        fixture.agent_control_http_url().await,
        Some(bearer.clone()),
        "tyde_send_agent_message".to_owned(),
        json!({"agent_id": start.agent_id, "message": "Retained global input", "interrupt": true, "global": true}),
    ).await;
    assert!(!failed, "idle explicit interrupt must retain the message");
    fixture
        .next_chat_event_matching(&agent, "global interruption retires question", |event| {
            matches!(event, protocol::ChatEvent::ToolExecutionCompleted(completion)
            if completion.tool_call_id == "entry-unanswered-question"
                && matches!(completion.outcome, protocol::ToolExecutionOutcome::Cancelled { .. }))
        })
        .await;
    let changed = expect_kind(
        &mut fixture.client,
        FrameKind::AgentProjectChanged,
        "workbench entry",
    )
    .await
    .parse_payload::<AgentStartPayload>()
    .unwrap();
    assert_eq!(changed.agent_id, start.agent_id);
    assert!(
        changed.session_id == start.session_id,
        "entry preserves the conversation"
    );
    assert_eq!(changed.project_id, Some(workbench_id.clone()));
    assert_eq!(changed.workspace_roots, roots);
    let broadcast = expect_kind(
        &mut observer,
        FrameKind::AgentProjectChanged,
        "second client entry",
    )
    .await
    .parse_payload::<AgentStartPayload>()
    .unwrap();
    assert_eq!(broadcast.project_id, changed.project_id);
    assert_eq!(broadcast.workspace_roots, changed.workspace_roots);
    continued.wait_until_entered().await;
    let before_dispatch = diagnostic_mock.requests().await;
    assert_eq!(
        before_dispatch
            .iter()
            .filter(|request| matches!(request, server::backend::mock::MockRequest::Input(_)))
            .count(),
        3,
        "only the system continuation may start before retained input"
    );
    assert!(!before_dispatch.iter().any(|request| matches!(request,
        server::backend::mock::MockRequest::Input(payload)
            if payload.message == "Retained global input" || payload.message == "Ordinary queued work")));

    let (_, after) = fixture.connect_with_bootstrap().await;
    let saved = after
        .sessions
        .iter()
        .find(|saved| Some(&saved.id) == start.session_id.as_ref())
        .unwrap();
    assert_eq!(saved.project_id, Some(workbench_id.clone()));
    assert_eq!(saved.workspace_roots, roots);
    continued.release_one();
    let continuation = fixture.finish_turn(&agent).await;
    assert!(
        continuation
            .chat_events()
            .iter()
            .any(|event| matches!(event,
        protocol::ChatEvent::MessageAdded(message)
            if matches!(message.sender, protocol::MessageSender::System)
                && message.origin == Some(protocol::MessageOrigin::System))),
        "Workbench entry continuation is a Tyde system message, not human input"
    );

    assert!(
        continuation
            .chat_events()
            .iter()
            .any(|event| matches!(event, protocol::ChatEvent::TypingStatusChanged(true))),
        "entry must start a client-visible continuation turn without user input"
    );
    assert!(
        continuation
            .chat_events()
            .iter()
            .any(|event| matches!(event,
        protocol::ChatEvent::StreamEnd(end)
            if matches!(end.message.sender, protocol::MessageSender::Assistant { .. })
                && end.message.content == "Working inside the workbench")),
        "the client must receive the automatic continuation's assistant output"
    );

    for _ in 0..2 {
        let delivered = fixture.finish_turn(&agent).await;
        let reported = delivered
            .chat_events()
            .into_iter()
            .find_map(|event| match event {
                protocol::ChatEvent::StreamEnd(end) => Some(end.message.content),
                _ => None,
            })
            .expect("destination input roots");
        assert!(
            serde_json::from_str::<Vec<String>>(&reported).unwrap() == roots,
            "retained input must execute only in destination"
        );
    }
    fixture.mock(&agent).await;
    let delivered_requests = diagnostic_mock.requests_after_commands().await;
    assert_eq!(
        delivered_requests
            .iter()
            .filter(|request| matches!(request, server::backend::mock::MockRequest::Interrupt))
            .count(),
        3
    );
    for message in ["Retained global input", "Ordinary queued work"] {
        assert_eq!(
            delivered_requests
                .iter()
                .filter(|request| matches!(request,
            server::backend::mock::MockRequest::Input(payload) if payload.message == message))
                .count(),
            1,
            "each retained payload must be dispatched exactly once"
        );
    }
    assert_eq!(ordinary.messages.len(), 1);

    // Each recovery surface needs its own unanswered card: the first
    // successful cancellation releases entry before the next delivery.
    for case in 0..4 {
        let question = MockGateHandle::new();
        let answer_turn = MockGateHandle::new();
        let continuation_gate = MockGateHandle::new();
        let question_id = format!("entry-recovery-question-{case}");
        let retained = format!("Retained recovery input {case}");
        let scheduled = case != 3;
        let typed_answer = case == 2;
        let destination = if case == 1 {
            workbench_id.clone()
        } else {
            parent.id.clone()
        };
        let destination_roots = if case == 1 {
            roots.clone()
        } else {
            project_roots(&parent)
        };
        let requests_before = diagnostic_mock.requests_after_commands().await.len();
        let mut turns = vec![MockTurn::async_question_request(&question_id, &question)];
        if typed_answer {
            turns.push(MockTurn::gated_text("Answer accepted", &answer_turn));
        }
        if scheduled {
            turns.push(MockTurn::gated_text(
                "Destination continuation",
                &continuation_gate,
            ));
        }
        turns.push(MockTurn::workspace_roots());
        diagnostic_mock.enqueue_all(turns).await;
        fixture
            .client
            .send_message(
                &agent.stream,
                "Post another asynchronous question".to_owned(),
            )
            .await
            .unwrap();
        question.wait_until_entered().await;
        fixture.next_chat_event_matching(&agent, "fresh unanswered card", |event| {
            matches!(event, protocol::ChatEvent::ToolRequest(request) if request.tool_call_id == question_id)
        }).await;
        if scheduled {
            let (failed, _) = call_agent_control(
                &fixture,
                &start.agent_id,
                "tyde_move_to_project",
                json!({"project_id": destination}),
            )
            .await;
            assert!(
                !failed,
                "entry must be scheduled without cancelling the card"
            );
        }
        question.release_one();
        fixture
            .next_chat_event_matching(&agent, "unanswered async question idle", |event| {
                matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
            })
            .await;
        let queued = if case == 1 || typed_answer {
            fixture
                .client
                .send_message(&agent.stream, retained.clone())
                .await
                .unwrap();
            Some(fixture.expect_queued_messages(&agent, 1).await)
        } else {
            None
        };
        fixture.mock(&agent).await;
        let held = diagnostic_mock.requests_after_commands().await;
        assert_eq!(
            held.len(),
            requests_before + 1,
            "idle question and pending move must not dispatch or cancel queued input"
        );
        match case {
            0 | 3 => {
                let (url, name, global) = if case == 0 {
                    (config_url.clone(), "tyde_config_send_agent_message", false)
                } else {
                    (
                        fixture.agent_control_http_url().await,
                        "tyde_send_agent_message",
                        true,
                    )
                };
                let mut arguments =
                    json!({"agent_id": start.agent_id, "message": retained, "interrupt": true});
                if global {
                    arguments["global"] = json!(true);
                }
                let (failed, _) = call_agent_control_request(
                    url,
                    Some(bearer.clone()),
                    name.to_owned(),
                    arguments,
                )
                .await;
                assert!(!failed, "explicit idle cancellation must retain its input");
            }
            1 => fixture
                .client
                .send_queued_message_now(
                    &agent.stream,
                    protocol::SendQueuedMessageNowPayload {
                        id: queued.as_ref().unwrap().messages[0].id.clone(),
                    },
                )
                .await
                .unwrap(),
            2 => fixture
                .client
                .send_message_payload(
                    &agent.stream,
                    protocol::SendMessagePayload {
                        message: "Answer".to_owned(),
                        images: None,
                        origin: None,
                        tool_response: Some(protocol::SendMessageToolResponse::AskUserQuestion {
                            tool_call_id: question_id.clone(),
                            answer: "Answer".to_owned(),
                            structured_answers: None,
                        }),
                    },
                )
                .await
                .unwrap(),
            _ => unreachable!(),
        }
        fixture.next_chat_event_matching(&agent, "question terminal outcome", |event| {
            matches!(event, protocol::ChatEvent::ToolExecutionCompleted(completion)
                if completion.tool_call_id == question_id && if typed_answer {
                    matches!(completion.outcome, protocol::ToolExecutionOutcome::Succeeded { .. })
                } else { matches!(completion.outcome, protocol::ToolExecutionOutcome::Cancelled { .. }) })
        }).await;
        if typed_answer {
            answer_turn.wait_until_entered().await;
            answer_turn.release_one();
            fixture.finish_turn(&agent).await;
        }
        if scheduled {
            let changed = expect_kind(
                &mut fixture.client,
                FrameKind::AgentProjectChanged,
                "question recovery entry",
            )
            .await
            .parse_payload::<AgentStartPayload>()
            .unwrap();
            assert!(changed.project_id == Some(destination.clone()));
            assert!(changed.workspace_roots == destination_roots);
            assert!(changed.session_id == start.session_id);
            let broadcast = expect_kind(
                &mut observer,
                FrameKind::AgentProjectChanged,
                "question recovery broadcast",
            )
            .await
            .parse_payload::<AgentStartPayload>()
            .unwrap();
            assert!(broadcast.project_id == Some(destination.clone()));
            continuation_gate.wait_until_entered().await;
            let admitted = diagnostic_mock.requests().await;
            let before_input = &admitted[requests_before..];
            assert_eq!(
                before_input
                    .iter()
                    .filter(|request| matches!(request,
                server::backend::mock::MockRequest::Input(payload) if payload.message == retained))
                    .count(),
                0,
                "retained input cannot run before the destination continuation"
            );
            assert_eq!(
                before_input
                    .iter()
                    .filter(|request| matches!(
                        request,
                        server::backend::mock::MockRequest::Interrupt
                    ))
                    .count(),
                usize::from(!typed_answer),
                "each explicit cancellation reaches Backend once; typed answers never interrupt"
            );
            let (_, persisted) = fixture.connect_with_bootstrap().await;
            assert!(
                persisted
                    .sessions
                    .iter()
                    .any(|session| Some(&session.id) == start.session_id.as_ref()
                        && session.project_id == Some(destination.clone())
                        && session.workspace_roots == destination_roots)
            );
            continuation_gate.release_one();
            fixture.finish_turn(&agent).await;
        }
        if !scheduled {
            fixture
                .next_chat_event_matching(&agent, "idle cancellation acknowledged", |event| {
                    matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
                })
                .await;
        }
        let delivered = fixture.finish_turn(&agent).await;
        let reported = delivered
            .chat_events()
            .into_iter()
            .find_map(|event| match event {
                protocol::ChatEvent::StreamEnd(end) => Some(end.message.content),
                _ => None,
            })
            .expect("recovered input workspace report");
        assert!(
            serde_json::from_str::<Vec<String>>(&reported).unwrap() == destination_roots,
            "recovered input must use actual destination roots"
        );
        fixture.mock(&agent).await;
        let final_requests = diagnostic_mock.requests_after_commands().await;
        let final_requests = &final_requests[requests_before..];
        assert_eq!(
            final_requests
                .iter()
                .filter(|request| matches!(request,
            server::backend::mock::MockRequest::Input(payload) if payload.message == retained))
                .count(),
            1,
            "each recovered input is dispatched exactly once"
        );
        assert_eq!(
            final_requests
                .iter()
                .filter(|request| matches!(request, server::backend::mock::MockRequest::Interrupt))
                .count(),
            usize::from(!typed_answer)
        );
        eprintln!(
            "ENTRY RECOVERY case={case} scheduled={scheduled} typed_answer={typed_answer} delivered_once=true destination_roots=true"
        );
    }

    let (mut replay_client, replay_host) = fixture.connect_with_bootstrap().await;
    let replay_stream = &replay_host
        .agents
        .iter()
        .find(|entry| entry.agent_id == start.agent_id)
        .expect("recovered agent descriptor")
        .instance_stream;
    let replay = fixture::next_frame_matching_on(
        &mut replay_client,
        "recovered conversation replay",
        |env| env.kind == FrameKind::AgentBootstrap && &env.stream == replay_stream,
    )
    .await
    .parse_payload::<protocol::AgentBootstrapPayload>()
    .unwrap();
    // AgentBootstrap is deliberately only the last 15 messages. Fetch its
    // typed prior-history cursor rather than claiming it is a full replay.
    let before_seq = replay
        .events
        .iter()
        .find_map(|event| match event {
            protocol::AgentBootstrapEvent::HasPriorHistory { before_seq, .. } => Some(*before_seq),
            _ => None,
        })
        .expect("coarse flow exceeds initial history tail");
    replay_client
        .fetch_session_history(
            replay_stream,
            protocol::FetchSessionHistoryPayload {
                agent_id: start.agent_id.clone(),
                request_id: protocol::HistoryPageRequestId(uuid::Uuid::new_v4().to_string()),
                before_seq: Some(before_seq),
                limit: 200,
            },
        )
        .await
        .unwrap();
    let prior =
        fixture::next_frame_matching_on(&mut replay_client, "recovered prior history", |env| {
            env.kind == FrameKind::SessionHistory && &env.stream == replay_stream
        })
        .await
        .parse_payload::<protocol::SessionHistoryPayload>()
        .unwrap();
    assert!(
        !prior.has_more_before,
        "bounded coarse flow history must be complete"
    );
    let replay_events: Vec<_> = prior
        .events
        .into_iter()
        .chain(replay.events.into_iter().filter_map(|event| match event {
            protocol::AgentBootstrapEvent::ChatEvent(event) => Some(event),
            _ => None,
        }))
        .collect();
    for message in [
        "Retained global input".to_owned(),
        "Ordinary queued work".to_owned(),
    ]
    .into_iter()
    .chain((0..4).map(|case| format!("Retained recovery input {case}")))
    {
        assert_eq!(
            replay_events
                .iter()
                .filter(|event| matches!(event,
            protocol::ChatEvent::MessageAdded(entry)
                if entry.content == message))
                .count(),
            1,
            "client replay must contain each recovered input exactly once"
        );
    }
    for (question_id, succeeded) in [("entry-unanswered-question".to_owned(), false)]
        .into_iter()
        .chain((0..4).map(|case| (format!("entry-recovery-question-{case}"), case == 2)))
    {
        let outcomes: Vec<_> = replay_events
            .iter()
            .filter_map(|event| match event {
                protocol::ChatEvent::ToolExecutionCompleted(completion)
                    if completion.tool_call_id == question_id =>
                {
                    Some(&completion.outcome)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes.len(),
            1,
            "client replay must retire each question exactly once"
        );
        assert!(if succeeded {
            matches!(
                outcomes[0],
                protocol::ToolExecutionOutcome::Succeeded { .. }
            )
        } else {
            matches!(
                outcomes[0],
                protocol::ToolExecutionOutcome::Cancelled { .. }
            )
        });
    }
    drop(replay_client);

    let unrelated_workbench =
        create_workbench(&mut fixture.client, &unrelated, "feature/cross-project").await;
    let mut previous_project = Some(parent.id.clone());
    let mut previous_roots = project_roots(&parent);
    for destination in [Some(&unrelated), None, Some(&unrelated_workbench)] {
        let finishing = MockGateHandle::new();
        fixture
            .mock(&agent)
            .await
            .enqueue_all([
                MockTurn::gated_text("Finishing before cross-project entry", &finishing),
                MockTurn::workspace_roots(),
            ])
            .await;
        fixture
            .client
            .send_message(
                &agent.stream,
                "Continue in the selected destination".to_owned(),
            )
            .await
            .unwrap();
        finishing.wait_until_entered().await;
        let project_id = destination.map(|project| project.id.clone());
        let expected_roots = destination.map_or_else(|| previous_roots.clone(), project_roots);
        let (failed, body) = call_agent_control(
            &fixture,
            &start.agent_id,
            "tyde_move_to_project",
            json!({"project_id": project_id}),
        )
        .await;
        eprintln!(
            "CROSS-PROJECT SELF MOVE: destination_assigned={} scheduled={} ancestry_rejection={}",
            project_id.is_some(),
            !failed,
            body.contains("Destination is outside caller project scope")
        );
        assert!(
            !failed,
            "an existing unrelated project/workbench or Home must be enterable"
        );
        let (_, pending) = fixture.connect_with_bootstrap().await;
        let pending = pending
            .agents
            .iter()
            .find(|entry| entry.agent_id == start.agent_id)
            .unwrap();
        assert_eq!(
            pending.project_id, previous_project,
            "self move must wait for turn completion"
        );
        assert_eq!(pending.workspace_roots, previous_roots);
        let (failed, _) = call_agent_control(
            &fixture,
            &start.agent_id,
            "tyde_review",
            json!({"request_id": "pending-cross-project"}),
        )
        .await;
        assert!(
            failed,
            "pending cross-project entry cannot review the source"
        );
        finishing.release_one();
        let moved = expect_kind(
            &mut fixture.client,
            FrameKind::AgentProjectChanged,
            "cross-project self entry",
        )
        .await
        .parse_payload::<AgentStartPayload>()
        .unwrap();
        assert_eq!(moved.project_id, project_id);
        assert_eq!(moved.workspace_roots, expected_roots);
        assert!(
            moved.session_id == start.session_id,
            "cross-project entry preserves conversation identity"
        );
        assert_eq!(moved.parent_agent_id, start.parent_agent_id);
        assert_eq!(moved.tyde_permissions, start.tyde_permissions);
        let broadcast = expect_kind(
            &mut observer,
            FrameKind::AgentProjectChanged,
            "cross-project observer",
        )
        .await
        .parse_payload::<AgentStartPayload>()
        .unwrap();
        assert_eq!(broadcast.project_id, project_id);
        assert_eq!(broadcast.workspace_roots, expected_roots);
        let turn = fixture.finish_turn(&agent).await;
        let reported = turn
            .chat_events()
            .into_iter()
            .find_map(|event| match event {
                protocol::ChatEvent::StreamEnd(end) => Some(end.message.content),
                _ => None,
            })
            .expect("continuation workspace report");
        let reported: Vec<String> = serde_json::from_str(&reported).expect("workspace roots JSON");
        assert_eq!(
            reported, expected_roots,
            "resumed execution must use the actual destination roots"
        );
        let (_, persisted) = fixture.connect_with_bootstrap().await;
        let saved = persisted
            .sessions
            .iter()
            .find(|saved| Some(&saved.id) == start.session_id.as_ref())
            .unwrap();
        assert_eq!(saved.project_id, project_id);
        assert_eq!(saved.workspace_roots, expected_roots);
        previous_project = project_id;
        previous_roots = expected_roots;
    }
    let review_project_id = unrelated_workbench.id.clone();
    let review_roots = project_roots(&unrelated_workbench);
    fs::write(
        Path::new(&review_roots[0]).join("README.md"),
        "# Cross-project workbench changes\n",
    )
    .unwrap();
    next_frame_matching_on(&mut fixture.client, "workbench reviewable changes", |env| {
        env.stream.0 == format!("/project/{}", review_project_id.0)
            && env.kind == FrameKind::ProjectGitStatus
            && env
                .parse_payload::<protocol::ProjectGitStatusPayload>()
                .unwrap()
                .roots
                .iter()
                .any(|root| {
                    root.root.0 == review_roots[0]
                        && root.files.iter().any(|file| {
                            file.relative_path == "README.md" && file.unstaged.is_some()
                        })
                })
    })
    .await;
    let reviewer_gate = MockGateHandle::new();
    let reviewer_reservation = fixture
        .reserve_next_mock_launch(
            "Review: Entry",
            MockScript::one(MockTurn::gated_text(
                "Reviewing the workbench",
                &reviewer_gate,
            )),
        )
        .await;
    let review_caller = fixture.agent_control_caller(&start.agent_id).await;
    let call = tokio::spawn(async move {
        call_agent_control_request(
            review_caller.await_url,
            Some(
                review_caller
                    .authorization
                    .strip_prefix("Bearer ")
                    .unwrap()
                    .to_owned(),
            ),
            "tyde_review".to_owned(),
            json!({"request_id": "entered-review"}),
        )
        .await
    });
    let reviewer = next_frame_matching_on(&mut fixture.client, "workbench reviewer", |env| {
        env.kind == FrameKind::NewAgent
            && env.parse_payload::<NewAgentPayload>().unwrap().name == "Review: Entry"
    })
    .await
    .parse_payload::<NewAgentPayload>()
    .unwrap();
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &reviewer.instance_stream,
        "entered project reviewer busy",
        |event| matches!(event, protocol::ChatEvent::TypingStatusChanged(true)),
    )
    .await;
    let (failed, summaries) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_list_reviews",
        json!({"limit": 1}),
    )
    .await;
    assert!(!failed);
    let summaries: Value = serde_json::from_str(&summaries).unwrap();
    let review_id = protocol::ReviewId(
        summaries["items"][0]["review_id"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    fixture
        .client
        .review_subscribe(&review_id, protocol::ReviewSubscribePayload::default())
        .await
        .unwrap();
    let snapshot = expect_kind(
        &mut fixture.client,
        FrameKind::ReviewBootstrap,
        "entered project review",
    )
    .await
    .parse_payload::<protocol::ReviewBootstrapPayload>()
    .unwrap()
    .review;
    assert_eq!(snapshot.project_id, review_project_id);
    assert!(
        snapshot
            .diffs
            .iter()
            .any(|diff| diff.root.0 == review_roots[0]),
        "review must freeze the workbench's actual changes"
    );
    assert_eq!(reviewer.project_id, Some(review_project_id.clone()));
    assert_eq!(reviewer.parent_agent_id, Some(start.agent_id.clone()));
    reviewer_gate.wait_until_entered().await;
    reviewer_gate.release_one();
    fixture::next_chat_event_matching_on(
        &mut fixture.client,
        &reviewer.instance_stream,
        "entered project reviewer idle",
        |event| matches!(event, protocol::ChatEvent::TypingStatusChanged(false)),
    )
    .await;
    fixture
        .client
        .close_agent(&reviewer.instance_stream)
        .await
        .unwrap();
    next_frame_matching_on(&mut fixture.client, "reviewer closed", |env| {
        env.kind == FrameKind::AgentClosed
    })
    .await;
    drop(reviewer_reservation);
    let (failed, body) = call.await.unwrap();
    assert!(!failed);
    let review: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(review["status"], "completed");

    let returned = MockGateHandle::new();
    fixture
        .mock(&agent)
        .await
        .enqueue(MockTurn::gated_text(
            "Back in the parent project",
            &returned,
        ))
        .await;
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_move_to_project",
        json!({"project_id": parent.id.0}),
    )
    .await;
    assert!(!failed, "return to parent failed");
    let moved = expect_kind(
        &mut fixture.client,
        FrameKind::AgentProjectChanged,
        "parent return",
    )
    .await
    .parse_payload::<AgentStartPayload>()
    .unwrap();
    assert_eq!(moved.project_id, Some(parent.id.clone()));
    assert_eq!(moved.workspace_roots, project_roots(&parent));
    assert!(
        moved.session_id == start.session_id,
        "return preserves the conversation"
    );
    returned.wait_until_entered().await;
    let (failed, body) = call_agent_control(&fixture, &start.agent_id, "tyde_create_workbench", json!({
        "parent_project_id": parent.id.0, "branch": "feature/coordinator", "action": "create_only"
    })).await;
    assert!(!failed, "create only failed");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["entry_scheduled"], false);
    let (_, unchanged) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        unchanged
            .agents
            .iter()
            .find(|entry| entry.agent_id == start.agent_id)
            .unwrap()
            .project_id,
        Some(parent.id.clone()),
        "coordinators must stay in their project"
    );
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_remove_workbench",
        json!({"project_id": workbench_id.0, "force": true}),
    )
    .await;
    assert!(!failed, "returning to parent must allow workbench cleanup");
    returned.release_one();
    fixture
        .next_chat_event_matching(&agent, "parent continuation idle", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
        })
        .await;

    let stop_question = MockGateHandle::new();
    fixture
        .mock(&agent)
        .await
        .enqueue(MockTurn::async_question_request(
            "entry-stop-question",
            &stop_question,
        ))
        .await;
    fixture
        .client
        .send_message(&agent.stream, "Continue in the parent".to_owned())
        .await
        .unwrap();
    fixture
        .next_chat_event_matching(&agent, "cancellable turn active", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(true))
        })
        .await;
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_move_to_project",
        json!({"project_id": created["project_id"]}),
    )
    .await;
    assert!(!failed, "schedule existing workbench entry");
    stop_question.wait_until_entered().await;
    stop_question.release_one();
    fixture
        .next_chat_event_matching(&agent, "stop question survives normal idle", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
        })
        .await;
    fixture.client.interrupt(&agent.stream).await.unwrap();
    let cancelled = expect_kind(
        &mut fixture.client,
        FrameKind::AgentError,
        "cancelled project entry",
    )
    .await
    .parse_payload::<protocol::AgentErrorPayload>()
    .unwrap();
    assert!(cancelled.message.contains("entry cancelled"));
    fixture
        .next_chat_event_matching(&agent, "Stop retires the unanswered question", |event| {
            matches!(event, protocol::ChatEvent::ToolExecutionCompleted(completion)
            if completion.tool_call_id == "entry-stop-question"
                && matches!(completion.outcome, protocol::ToolExecutionOutcome::Cancelled { .. }))
        })
        .await;
    eprintln!("ENTRY DIAGNOSTIC Stop while idle: move_cancelled=true question_cancelled=true");
    fixture
        .next_chat_event_matching(&agent, "cancelled turn idle", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
        })
        .await;
    let (_, cancelled) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        cancelled
            .agents
            .iter()
            .find(|entry| entry.agent_id == start.agent_id)
            .unwrap()
            .project_id,
        Some(parent.id.clone()),
        "Stop must cancel entry and its automatic continuation"
    );

    let removal = MockGateHandle::new();
    fixture
        .mock(&agent)
        .await
        .enqueue_all([
            MockTurn::gated_text("Current turn before removed destination", &removal),
            MockTurn::text("Reporting the explicit entry failure"),
        ])
        .await;
    fixture
        .client
        .send_message(&agent.stream, "Check destination removal".to_owned())
        .await
        .unwrap();
    removal.wait_until_entered().await;
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_move_to_project",
        json!({"project_id": created["project_id"]}),
    )
    .await;
    assert!(!failed, "schedule entry before removal");
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: ProjectId(created["project_id"].as_str().unwrap().to_owned()),
            force: false,
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut fixture.client, "pending destination removed", |env| {
        env.kind == FrameKind::ProjectNotify && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(),
            ProjectNotifyPayload::Delete { project } if project.id.0 == created["project_id"].as_str().unwrap())
    }).await;
    removal.release_one();
    let failed = expect_kind(
        &mut fixture.client,
        FrameKind::AgentError,
        "removed destination entry error",
    )
    .await
    .parse_payload::<protocol::AgentErrorPayload>()
    .unwrap();
    assert!(failed.message.contains("removed before entry"));
    assert!(!failed.fatal);
    fixture.finish_turn(&agent).await;
    let (_, unchanged) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        unchanged
            .agents
            .iter()
            .find(|entry| entry.agent_id == start.agent_id)
            .unwrap()
            .project_id,
        Some(parent.id.clone()),
        "failed entry must never relabel the agent"
    );

    use server::backend::WorkspaceRelocationError;

    let destination = create_workbench(&mut fixture.client, &parent, "feature/readiness").await;
    let mock = fixture.mock(&agent).await;
    let rejecting = MockGateHandle::new();
    mock.enqueue_all([
        MockTurn::gated_text("Finishing before rejected relocation", &rejecting),
        MockTurn::text("Reporting the rejected relocation"),
    ])
    .await;
    mock.set_workspace_relocation_results(vec![
        Err(WorkspaceRelocationError::Rejected(
            "Destination rejected before mutation".to_owned(),
        )),
        Err(WorkspaceRelocationError::Rejected(
            "Restoration was never admissible".to_owned(),
        )),
    ])
    .await;
    fixture
        .client
        .send_message(&agent.stream, "Try the rejected destination".to_owned())
        .await
        .unwrap();
    rejecting.wait_until_entered().await;
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_move_to_project",
        json!({"project_id": destination.id.0}),
    )
    .await;
    assert!(!failed, "schedule preflight rejection");
    rejecting.release_one();
    let rejected = expect_kind(
        &mut fixture.client,
        FrameKind::AgentError,
        "preflight rejection remains recoverable",
    )
    .await
    .parse_payload::<protocol::AgentErrorPayload>()
    .unwrap();
    assert!(
        !rejected.fatal,
        "a rejected move cannot terminate the conversation"
    );
    assert!(rejected.message.contains("before mutation"));
    fixture.finish_turn(&agent).await;
    let (_, unchanged) = fixture.connect_with_bootstrap().await;
    let live = unchanged
        .agents
        .iter()
        .find(|entry| entry.agent_id == start.agent_id)
        .expect("rejected agent is still alive");
    assert_eq!(live.project_id, Some(parent.id.clone()));
    assert_eq!(live.workspace_roots, project_roots(&parent));
    let saved = unchanged
        .sessions
        .iter()
        .find(|session| Some(&session.id) == start.session_id.as_ref())
        .unwrap();
    assert_eq!(saved.project_id, Some(parent.id.clone()));
    assert_eq!(saved.workspace_roots, project_roots(&parent));

    let finishing = MockGateHandle::new();
    let background = MockGateHandle::new();
    let relocated = MockGateHandle::new();
    mock.enqueue_all([
        MockTurn::gated_text(
            "Finishing before native completion continuation",
            &finishing,
        ),
        MockTurn::gated_text("Native background completion continuation", &background),
        MockTurn::gated_text("Continuing in the relocated workspace", &relocated),
    ])
    .await;
    mock.set_workspace_relocation_results(vec![Err(WorkspaceRelocationError::Busy), Ok(())])
        .await;
    fixture
        .client
        .send_message(
            &agent.stream,
            "Move after background work settles".to_owned(),
        )
        .await
        .unwrap();
    finishing.wait_until_entered().await;
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_move_to_project",
        json!({"project_id": destination.id.0}),
    )
    .await;
    assert!(
        !failed,
        "schedule relocation across native background continuation"
    );
    finishing.release_one();
    background.wait_until_entered().await;
    let (_, pending) = fixture.connect_with_bootstrap().await;
    let live = pending
        .agents
        .iter()
        .find(|entry| entry.agent_id == start.agent_id)
        .expect("busy agent is still alive");
    assert_eq!(
        live.project_id,
        Some(parent.id.clone()),
        "busy move cannot relabel the conversation"
    );
    assert_eq!(live.workspace_roots, project_roots(&parent));
    let (failed, _) = call_agent_control(
        &fixture,
        &start.agent_id,
        "tyde_review",
        json!({"request_id": "workbench-review"}),
    )
    .await;
    assert!(
        failed,
        "busy relocation must retain its movement reservation"
    );
    background.release_one();
    let moved = expect_kind(
        &mut fixture.client,
        FrameKind::AgentProjectChanged,
        "relocation after background continuation",
    )
    .await
    .parse_payload::<AgentStartPayload>()
    .unwrap();
    assert_eq!(moved.project_id, Some(destination.id.clone()));
    assert_eq!(moved.workspace_roots, project_roots(&destination));
    assert!(
        moved.session_id == start.session_id,
        "native continuation relocation preserves conversation identity"
    );
    relocated.wait_until_entered().await;
    relocated.release_one();
    fixture.finish_turn(&agent).await;

    eprintln!("WORKSPACE MOVE PHASE: uncertain phase begin");
    for restoration_succeeds in [true, false] {
        let finishing = MockGateHandle::new();
        mock.enqueue(MockTurn::gated_text(
            "Finishing before uncertain relocation",
            &finishing,
        ))
        .await;
        if restoration_succeeds {
            mock.enqueue(MockTurn::workspace_roots()).await;
        }
        mock.set_workspace_relocation_results(vec![
            Err(WorkspaceRelocationError::OutcomeUnknown(
                "Native relocation outcome was not confirmed".to_owned(),
            )),
            if restoration_succeeds {
                Ok(())
            } else {
                Err(WorkspaceRelocationError::Rejected(
                    "Original workspace could not be restored".to_owned(),
                ))
            },
        ])
        .await;
        fixture
            .client
            .send_message(&agent.stream, "Try the uncertain destination".to_owned())
            .await
            .unwrap();
        finishing.wait_until_entered().await;
        let (failed, _) = call_agent_control(
            &fixture,
            &start.agent_id,
            "tyde_move_to_project",
            json!({"project_id": parent.id.0}),
        )
        .await;
        assert!(!failed, "schedule uncertain relocation");
        finishing.release_one();
        let error = expect_kind(
            &mut fixture.client,
            FrameKind::AgentError,
            "uncertain relocation",
        )
        .await
        .parse_payload::<protocol::AgentErrorPayload>()
        .unwrap();
        assert_eq!(
            error.fatal, !restoration_succeeds,
            "only an unreconciled native workspace can terminate the conversation"
        );
        let state = if restoration_succeeds {
            eprintln!("WORKSPACE MOVE PHASE: await native workspace report");
            let reported = fixture.finish_turn(&agent).await;
            let effective = reported
                .frames
                .iter()
                .filter(|frame| frame.kind == FrameKind::ChatEvent)
                .map(|frame| frame.parse_payload::<protocol::ChatEvent>().unwrap())
                .find_map(|event| match event {
                    protocol::ChatEvent::StreamEnd(end) => Some(end.message.content),
                    _ => None,
                })
                .expect("native workspace report");
            let effective: Vec<String> =
                serde_json::from_str(&effective).expect("native workspace roots JSON");
            assert_eq!(
                effective,
                project_roots(&destination),
                "restoration must repair the effective native workspace, not just server metadata"
            );
            fixture.connect_with_bootstrap().await.1
        } else {
            eprintln!("WORKSPACE MOVE PHASE: await native shutdown");
            shutdown.wait_until_entered().await;
            eprintln!("WORKSPACE MOVE PHASE: native shutdown entered");
            // Bootstrap attaches to the actor, which cannot serve commands while
            // its native shutdown is gated. Release that boundary before attaching.
            shutdown.release_one();
            let (_, stopped) = fixture.connect_with_bootstrap().await;
            assert!(
                stopped
                    .agents
                    .iter()
                    .any(|entry| entry.agent_id == start.agent_id),
                "internal shutdown does not remove the stopped card"
            );
            eprintln!("WORKSPACE MOVE PHASE: await mock cleanliness after release");
            mock.assert_clean().await;
            eprintln!("WORKSPACE MOVE PHASE: restart after internal shutdown");
            let restored = server::new_spawn_operation_test_gate();
            let restarted = fixture
                .restart_host_with_runtime_config_before_connect(
                    |config| config.restoration_complete_test_gate = Some(restored.shared()),
                    async {
                        restored.wait_until_entered().await;
                        restored.release_one();
                    },
                )
                .await;
            assert!(
                !restarted
                    .agents
                    .iter()
                    .any(|entry| entry.agent_id == start.agent_id
                        || entry.session_id == start.session_id),
                "fatal internal shutdown must withdraw recovery before host restart"
            );
            restarted
        };
        let live = state
            .agents
            .iter()
            .find(|entry| entry.agent_id == start.agent_id);
        assert_eq!(live.is_some(), restoration_succeeds);
        if let Some(live) = live {
            assert_eq!(live.project_id, Some(destination.id.clone()));
            assert_eq!(live.workspace_roots, project_roots(&destination));
        }
        let saved = state
            .sessions
            .iter()
            .find(|session| Some(&session.id) == start.session_id.as_ref())
            .unwrap();
        assert_eq!(saved.project_id, Some(destination.id.clone()));
        assert_eq!(saved.workspace_roots, project_roots(&destination));
        assert!(
            saved.resumable,
            "failed relocation must preserve accessible saved history"
        );
    }
    mock.assert_clean().await;
}

#[tokio::test]
async fn bubblewrap_engine_settings_terminal_boundary_and_replay() {
    use protocol::{WorkbenchAvailability, WorkbenchEngine, WorkbenchEnvironment};

    let mut fixture = Fixture::new().await;
    fixture
        .host_for_test()
        .set_session_schema_ready_for_test(BackendKind::Codex)
        .await;
    assert_eq!(
        fixture.bootstrap.settings.workbenches.engine,
        WorkbenchEngine::Native
    );
    let repo = init_git_repo("bubblewrap-boundary");
    fs::write(repo.path().join(".gitignore"), "/build-output/\n").expect("ignore build output");
    git(repo.path(), &["add", ".gitignore"]);
    git(
        repo.path(),
        &["commit", "-m", "Ignore private build output"],
    );
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let write_id = fixture
        .client
        .replace_setting(
            "/workbenches/engine",
            WorkbenchEngine::Bubblewrap,
            WorkbenchEngine::Native,
        )
        .await
        .expect("write engine setting with CAS");
    let result = next_frame_matching_on(&mut fixture.client, "engine setting result", |env| {
        env.kind == FrameKind::SettingsWriteResult
            && env
                .parse_payload::<protocol::SettingsWriteResultPayload>()
                .unwrap()
                .write_id
                == write_id
    })
    .await
    .parse_payload::<protocol::SettingsWriteResultPayload>()
    .unwrap();
    assert!(
        result.applied && result.field_errors.is_empty(),
        "engine setting must apply cleanly"
    );
    let (_, configured) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        configured.settings.workbenches.engine,
        WorkbenchEngine::Bubblewrap
    );
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("feature/bubblewrap-boundary".to_owned()),
            name: "Bubblewrap boundary".to_owned(),
        })
        .await
        .expect("request real engine workbench");
    let created = next_frame_matching_on(&mut fixture.client, "engine create outcome", |env| {
        matches!(env.kind, FrameKind::ProjectNotify | FrameKind::CommandError)
    })
    .await;
    if created.kind == FrameKind::CommandError {
        let error = created.parse_payload::<CommandErrorPayload>().unwrap();
        assert_eq!(error.request_kind, FrameKind::WorkbenchCreate);
        assert_eq!(error.code, CommandErrorCode::InvalidInput);
        assert!(!error.fatal);
        match &configured.workbench_support.bubblewrap {
            WorkbenchAvailability::Unsupported { reason, message } => {
                assert_ne!(*reason, protocol::WorkbenchUnsupportedReason::NotProbed);
                assert_eq!(error.workbench_reason, Some(*reason));
                assert!(
                    !message.is_empty() && error.message.contains(message),
                    "unsupported engine must report its probed prerequisite failure"
                );
                eprintln!("Bubblewrap boundary explicitly unsupported: {reason:?}");
            }
            WorkbenchAvailability::Available => {
                assert_eq!(
                    error.workbench_reason,
                    Some(protocol::WorkbenchUnsupportedReason::ReflinkUnsupported)
                );
                assert!(
                    error.message.contains("Strict CoW cloning failed")
                        || error.message.contains("CoW probe"),
                    "available namespace may reject storage only with an explicit CoW failure"
                );
                eprintln!("Bubblewrap boundary explicitly unsupported: ReflinkUnsupported");
            }
        }
        let (_, replay) = fixture.connect_with_bootstrap().await;
        assert!(
            !replay
                .projects
                .iter()
                .any(|project| project.parent_project_id() == Some(&parent.id)),
            "failed setup must not register a Native substitute"
        );
        assert!(
            !expected_worktree_path(repo.path(), "feature-bubblewrap-boundary").exists(),
            "unsupported setup must roll back its real worktree"
        );
        assert_eq!(
            replay.settings.workbenches.engine,
            WorkbenchEngine::Bubblewrap
        );
        return;
    }
    let ProjectNotifyPayload::Upsert { project } = created.parse_payload().unwrap() else {
        panic!("engine create must upsert its authoritative project");
    };
    let ProjectSource::GitWorkbench { environment, .. } = &project.source else {
        panic!("engine create must produce a Git workbench");
    };
    let WorkbenchEnvironment::Bubblewrap {
        manifest_path,
        logical_root,
        initialization,
        ..
    } = environment
    else {
        panic!("Bubblewrap selection cannot route through Native");
    };
    assert_eq!(logical_root.0, "/mnt/tyde-workspace");
    assert!(
        matches!(initialization, protocol::WorkbenchInitialization::Blocked { message } if message.contains("No conventional"))
    );
    let manifest_path = PathBuf::from(&manifest_path.0);
    let root = project.root_paths()[0].clone();
    let (caller, caller_start) = fixture
        .spawn_with(SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("execution-path-caller".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&project),
                prompt: "workbench caller".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: BackendAccessMode::Unrestricted,
                session_settings: None,
            },
        })
        .await;
    assert!(
        caller_start.session_id.is_some(),
        "MCP caller must acknowledge its persisted session before spawning a child"
    );
    fixture.finish_turn(&caller).await;
    let caller = caller.new_agent;
    eprintln!("BUBBLEWRAP FLOW MCP caller durable-and-idle");
    for (path, cause) in [
        (
            "/mnt/tyde-workspace/../boundary-outside",
            "parent traversal",
        ),
        (
            "/mnt/tyde-workspace/dir/../../boundary-outside",
            "parent traversal",
        ),
        ("/mnt/tyde-runtime/private", "Engine-only"),
        ("/mnt//tyde-runtime/private", "Engine-only"),
        ("/mnt/tyde-producer-receipts", "Engine-only"),
        ("/mnt", "Engine-only"),
    ] {
        let (is_error, body) = call_agent_control(
            &fixture,
            &caller.agent_id,
            "tyde_spawn_agent",
            json!({
                "workspace_roots": [path],
                "prompt": "must not register an execution-only host root",
                "backend_kind": "codex"
            }),
        )
        .await;
        assert!(
            is_error && body.contains(cause),
            "MCP must reject unrepresentable host paths before spawning"
        );
    }
    let (is_error, _) = call_agent_control(
        &fixture,
        &caller.agent_id,
        "tyde_spawn_agent",
        json!({
            "workspace_roots": ["/mnt//tyde-workspace/./"],
            "prompt": "inherit the explicitly mapped host workspace",
            "backend_kind": "codex"
        }),
    )
    .await;
    assert!(!is_error, "MCP must map an ordinary logical workspace root");
    let child = expect_kind(&mut fixture.client, FrameKind::NewAgent, "mapped MCP child")
        .await
        .parse_payload::<NewAgentPayload>()
        .expect("mapped MCP child payload");
    assert_eq!(child.workspace_roots, vec![root.0.clone()]);
    assert_eq!(child.project_id, Some(project.id.clone()));
    assert!(
        child.parent_agent_id.as_ref() == Some(&caller.agent_id),
        "MCP child must preserve its authenticated caller as parent"
    );
    let (_, replay) = fixture.connect_with_bootstrap().await;
    assert!(
        replay.projects.iter().any(|entry| entry == &project),
        "replay must carry the same typed environment and host root"
    );
    fixture
        .client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: project.id.clone(),
                root: root.clone(),
                relative_cwd: None,
            },
            cols: 120,
            rows: 30,
        })
        .await
        .expect("create real engine terminal");
    let terminal = expect_kind(
        &mut fixture.client,
        FrameKind::NewTerminal,
        "engine terminal",
    )
    .await
    .parse_payload::<NewTerminalPayload>()
    .unwrap();
    let bootstrap = expect_kind(
        &mut fixture.client,
        FrameKind::TerminalBootstrap,
        "engine terminal bootstrap",
    )
    .await
    .parse_payload::<protocol::TerminalBootstrapPayload>()
    .unwrap();
    assert_eq!(bootstrap.start.project_id, Some(project.id.clone()));
    assert_eq!(bootstrap.start.root, Some(root.clone()));
    let host_namespace = fs::read_link("/proc/self/ns/mnt")
        .expect("host namespace")
        .to_string_lossy()
        .into_owned();
    let script = format!(
        r#"import json, os, pathlib, subprocess
root = pathlib.Path('/mnt/tyde-workspace')
source = root / 'README.md'
output = root / 'build-output'
output.mkdir()
clone = output / 'private-copy.txt'
before = source.read_text()
with open('/dev/null', 'r+b', buffering=0) as device:
    sample = b'native device probe'
    devices = device.write(sample) == len(sample) and device.read(1) == b''
subprocess.run(['cp', '--reflink=always', str(source), str(clone)], check=True)
independent = clone.stat().st_ino != source.stat().st_ino
clone.write_text('private artifact\n')
private = independent and source.read_text() == before
source.write_text('edited in engine terminal\n')
# Frame machine-readable output after Bash's bracketed-paste CSI plus bare CR.
print('\n' + json.dumps({{'boundary': 'complete', 'cwd': os.getcwd() == str(root), 'namespace': os.readlink('/proc/self/ns/mnt') != {host_namespace:?}, 'cow': private, 'devices': devices, 'generic': not (root / 'Cargo.toml').exists() and os.environ.get('TYDE_WORKBENCH_ROOT') == str(root)}}))
"#
    );
    fixture
        .client
        .terminal_send(
            &terminal.terminal_id,
            protocol::TerminalSendPayload {
                data: format!("python3 - <<'TYDE_BOUNDARY_PY'\n{script}TYDE_BOUNDARY_PY\n"),
            },
        )
        .await
        .expect("run namespace boundary command");
    let mut output = String::new();
    loop {
        let event = next_frame_matching_on(&mut fixture.client, "engine command output", |env| {
            env.stream == terminal.stream
                && matches!(
                    env.kind,
                    FrameKind::TerminalOutput | FrameKind::TerminalError | FrameKind::TerminalExit
                )
        })
        .await;
        assert_eq!(
            event.kind,
            FrameKind::TerminalOutput,
            "terminal must remain running through command completion"
        );
        output.push_str(
            &event
                .parse_payload::<protocol::TerminalOutputPayload>()
                .unwrap()
                .data,
        );
        if let Some(evidence) = output
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["boundary"] == "complete")
        {
            assert!(
                ["cwd", "namespace", "cow", "devices", "generic"]
                    .iter()
                    .all(|key| evidence[*key].as_bool() == Some(true)),
                "actual terminal output must establish every isolation boundary"
            );
            break;
        }
    }
    assert!(
        fs::read_to_string(Path::new(&root.0).join("README.md"))
            .expect("terminal-edited host worktree")
            == "edited in engine terminal\n"
    );
    assert!(
        fs::read_to_string(repo.path().join("README.md")).expect("parent source")
            == "# bubblewrap-boundary\n",
        "workbench writes must not alter the parent checkout"
    );
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: project.id.clone(),
            force: true,
        })
        .await
        .expect("remove engine workbench and terminal");
    next_frame_matching_on(&mut fixture.client, "engine removed", |env| {
        env.kind == FrameKind::ProjectNotify
            && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(),
            ProjectNotifyPayload::Delete { project: deleted } if deleted.id == project.id)
    })
    .await;
    assert!(
        !Path::new(&root.0).exists(),
        "remove must finish host worktree cleanup"
    );
    assert!(!manifest_path.exists(), "remove must finish engine cleanup");
    let (_, replay) = fixture.connect_with_bootstrap().await;
    assert!(!replay.projects.iter().any(|entry| entry.id == project.id));
    assert!(replay.projects.iter().any(|entry| entry.id == parent.id));
    assert_eq!(
        replay.settings.workbenches.engine,
        WorkbenchEngine::Bubblewrap
    );
    eprintln!("BUBBLEWRAP FLOW initial-terminal-and-removal verified");
    let snapshots = repo.path().join(".git/tyde-workbench-snapshots-v2");
    assert!(
        fs::read_dir(&snapshots)
            .expect("snapshot directory")
            .next()
            .is_none(),
        "an unconfigured directory without an explicit producer cannot publish a snapshot"
    );

    fs::create_dir(repo.path().join(".tyde")).expect("project configuration directory");
    fs::write(
        repo.path().join("requirements.lock"),
        "opaque-project-input-v1\n",
    )
    .expect("tracked project-relative provenance input");
    fs::write(repo.path().join("assemble.py"),
        "from pathlib import Path\np = Path('build-output'); p.mkdir()\n(p / 'result.data').write_bytes(Path('README.md').read_bytes())\n").expect("non-Rust producer source");
    let python = tyde_process_env::find_executable_in_path("python3")
        .expect("the successful engine terminal established host Python availability");
    fs::write(
        repo.path().join(".tyde/workbench.json"),
        serde_json::to_vec(&json!({
            "environment": {
                "PATH": "${PATH}:/tyde-qa-provenance",
                "QA_PRODUCER_PATH": "/mnt/tyde-runtime/bin:${PATH}:/tyde-qa-provenance"
            },
            "snapshot": {
                "producer": ["python3", "assemble.py"],
                "include": ["build-output"],
                "provenance_paths": [python, "requirements.lock"],
                "provenance_environment": ["PATH"]
            }
        }))
        .unwrap(),
    )
    .expect("explicit generic project configuration");
    git(repo.path(), &["add", "."]);
    git(
        repo.path(),
        &["commit", "-m", "Configure opaque project artifacts"],
    );
    let caller = spawn_project_caller(
        &mut fixture.client,
        &parent,
        "snapshot caller",
        BackendAccessMode::Unrestricted,
    )
    .await;
    let (failed, response) = call_agent_control(&fixture, &caller.agent_id, "tyde_create_workbench", json!({
        "parent_project_id": parent.id.0, "branch": "feature/generic-producer", "action": "create_only"
    })).await;
    assert!(!failed, "MCP must use the selected server-owned engine");
    let response: Value = serde_json::from_str(&response).expect("MCP creation result");
    assert_eq!(response["environment"]["kind"], "bubblewrap");
    assert_eq!(response["environment"]["initialization"]["kind"], "cold");
    let producer_id = ProjectId(response["project_id"].as_str().unwrap().to_owned());
    let producer = fixture
        .connect_with_bootstrap()
        .await
        .1
        .projects
        .into_iter()
        .find(|project| project.id == producer_id)
        .expect("canonical MCP-created project");
    let producer_root = producer.root_paths()[0].clone();
    fs::File::open(Path::new(&producer_root.0).join("requirements.lock"))
        .expect("tracked producer input")
        .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .expect("fixed provenance-input timestamp distinct from a fresh checkout");
    assert!(
        !Path::new(&producer_root.0).join("build-output").exists(),
        "creation must not run a build"
    );
    fixture
        .client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: producer_id.clone(),
                root: producer_root.clone(),
                relative_cwd: None,
            },
            cols: 120,
            rows: 30,
        })
        .await
        .expect("generic producer terminal");
    let producer_terminal = expect_kind(
        &mut fixture.client,
        FrameKind::NewTerminal,
        "producer terminal",
    )
    .await
    .parse_payload::<NewTerminalPayload>()
    .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::TerminalBootstrap,
        "producer terminal bootstrap",
    )
    .await;
    fixture.client.terminal_send(&producer_terminal.terminal_id, protocol::TerminalSendPayload {
        data: "PATH=\"$QA_PRODUCER_PATH\" /mnt/tyde-runtime/bin/tyde-workbench-produce && printf '\\n{\"producer_completed\":true}\\n'\n".to_owned(),
    }).await.expect("explicit generic producer request");
    eprintln!("BUBBLEWRAP FLOW explicit-producer requested");
    let mut produced = String::new();
    loop {
        let event =
            next_frame_matching_on(&mut fixture.client, "producer command completion", |env| {
                env.stream == producer_terminal.stream
                    && matches!(
                        env.kind,
                        FrameKind::TerminalOutput
                            | FrameKind::TerminalError
                            | FrameKind::TerminalExit
                    )
            })
            .await;
        assert_eq!(
            event.kind,
            FrameKind::TerminalOutput,
            "producer command must complete successfully"
        );
        produced.push_str(
            &event
                .parse_payload::<protocol::TerminalOutputPayload>()
                .unwrap()
                .data,
        );
        assert!(
            !produced.contains("\"error\": {\"reason\":"),
            "explicit producer emitted a typed runtime failure before completion"
        );
        if produced
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|value| value["producer_completed"] == true)
        {
            break;
        }
    }
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: producer_id.clone(),
            force: false,
        })
        .await
        .expect("seal completed producer and remove resources");
    let removed = next_frame_matching_on(&mut fixture.client, "producer removed", |env| {
        env.kind == FrameKind::CommandError
            || (env.kind == FrameKind::ProjectNotify
                && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(),
            ProjectNotifyPayload::Delete { project } if project.id == producer_id))
    })
    .await;
    assert_eq!(
        removed.kind,
        FrameKind::ProjectNotify,
        "quiescent completed producer must seal successfully, not emit CommandError"
    );
    eprintln!("BUBBLEWRAP FLOW quiescent-producer removed");
    let cohorts: Vec<_> = fs::read_dir(&snapshots)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        cohorts.len(),
        1,
        "one completed quiescent producer must publish one actual cohort"
    );
    let (failed, response) = call_agent_control(&fixture, &caller.agent_id, "tyde_create_workbench", json!({
        "parent_project_id": parent.id.0, "branch": "feature/generic-consumer", "action": "create_only"
    })).await;
    assert!(
        !failed,
        "compatible MCP creation must clone opaque artifacts"
    );
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response["environment"]["initialization"]["kind"],
        "snapshot"
    );
    let consumer_id = ProjectId(response["project_id"].as_str().unwrap().to_owned());
    let consumer_root = PathBuf::from(response["roots"][0]["worktree_root"].as_str().unwrap());
    let result = consumer_root.join("build-output/result.data");
    let donor = cohorts[0].join("files/build-output/result.data");
    assert_eq!(
        fs::read(&result).unwrap(),
        fs::read(repo.path().join("README.md")).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            fs::metadata(&result).unwrap().ino(),
            fs::metadata(&donor).unwrap().ino()
        );
    }
    fs::write(&result, "private consumer edit").unwrap();
    assert_eq!(
        fs::read(&donor).unwrap(),
        fs::read(repo.path().join("README.md")).unwrap()
    );
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: consumer_id.clone(),
            force: true,
        })
        .await
        .expect("remove private consumer");
    next_frame_matching_on(&mut fixture.client, "consumer removed", |env| {
        env.kind == FrameKind::ProjectNotify
            && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(),
            ProjectNotifyPayload::Delete { project } if project.id == consumer_id)
    })
    .await;
    assert!(!consumer_root.exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bubblewrap_service_failure_after_foreground_exit_is_not_success() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut fixture = Fixture::new().await;
    assert!(
        matches!(
            fixture.bootstrap.workbench_support.bubblewrap,
            protocol::WorkbenchAvailability::Available
        ),
        "service lifecycle regression requires Linux Bubblewrap with unprivileged user/mount namespaces; capability={:?}",
        fixture.bootstrap.workbench_support.bubblewrap
    );
    let control = tempfile::Builder::new()
        .prefix("tyde-service-")
        .tempdir_in("/tmp")
        .expect("owned control directory");
    let service_listener = tokio::net::UnixListener::bind(control.path().join("service"))
        .expect("service exit control");
    let descendant_listener = tokio::net::UnixListener::bind(control.path().join("descendant"))
        .expect("descendant event control");
    let repo = init_git_repo("service-failure");
    fs::create_dir(repo.path().join(".tyde")).expect("project configuration directory");
    fs::write(repo.path().join(".tyde/workbench.json"), serde_json::to_vec(&json!({"environment":{"CONTROL_DIR":control.path()},"services":[{"name":"required","command":["python3","service.py"],"environment":{},"ready_notification":"ready.sock","stop_command":[]}]})).unwrap()).expect("generic service configuration");
    fs::write(
        repo.path().join("service.py"),
        r#"import os,pathlib,socket
runtime=pathlib.Path(os.environ['TYDE_WORKBENCH_RUNTIME'])
(runtime/'service.pid').write_text(str(os.getpid()))
with socket.socket(socket.AF_UNIX) as control:
 control.connect(os.environ['CONTROL_DIR']+'/service')
 with socket.socket(socket.AF_UNIX) as ready:
  ready.connect(str(runtime/'ready.sock'));ready.sendall(b'actual-ready')
 assert control.recv(1)==b'X'
"#,
    )
    .expect("real foreground service");
    fs::write(repo.path().join("command.py"), r#"import os,pathlib,select,socket
read,write=os.pipe()
if os.fork():
 os.close(write);assert os.read(read,1)==b'R';os._exit(0)
os.close(read)
parent=os.pidfd_open(os.getppid())
service=os.pidfd_open(int(pathlib.Path(os.environ['TYDE_WORKBENCH_RUNTIME']+'/service.pid').read_text()))
os.write(write,b'R');os.close(write)
select.select([parent],[],[])
with socket.socket(socket.AF_UNIX) as control:
 control.connect(os.environ['CONTROL_DIR']+'/descendant');control.sendall(b'P')
 select.select([service],[],[])
os._exit(0)
"#).expect("command with real owned descendant");
    git(repo.path(), &["add", "."]);
    git(
        repo.path(),
        &["commit", "-m", "Explicit service failure workload"],
    );
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let write = fixture
        .client
        .replace_setting(
            "/workbenches/engine",
            protocol::WorkbenchEngine::Bubblewrap,
            protocol::WorkbenchEngine::Native,
        )
        .await
        .expect("select engine");
    let result = next_frame_matching_on(&mut fixture.client, "service engine setting", |e| {
        e.kind == FrameKind::SettingsWriteResult
            && e.parse_payload::<protocol::SettingsWriteResultPayload>()
                .unwrap()
                .write_id
                == write
    })
    .await;
    assert!(
        result
            .parse_payload::<protocol::SettingsWriteResultPayload>()
            .unwrap()
            .applied
    );
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id,
            branch: GitBranchName("qa-service-failure".into()),
            name: "Service failure".into(),
        })
        .await
        .expect("create service workbench");
    let result = next_frame_matching_on(&mut fixture.client, "service workbench", |e| {
        matches!(e.kind, FrameKind::ProjectNotify | FrameKind::CommandError)
    })
    .await;
    assert_eq!(
        result.kind,
        FrameKind::ProjectNotify,
        "service lifecycle regression requires a strict-CoW filesystem; creation must succeed, reason={:?}",
        (result.kind == FrameKind::CommandError).then(|| result
            .parse_payload::<CommandErrorPayload>()
            .unwrap()
            .workbench_reason)
    );
    let ProjectNotifyPayload::Upsert { project } = result.parse_payload().unwrap() else {
        panic!("workbench upsert required")
    };
    let root = project.root_paths()[0].clone();
    fixture
        .client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: project.id.clone(),
                root: root.clone(),
                relative_cwd: None,
            },
            cols: 100,
            rows: 25,
        })
        .await
        .expect("real engine terminal");
    let terminal = expect_kind(
        &mut fixture.client,
        FrameKind::NewTerminal,
        "service terminal",
    )
    .await
    .parse_payload::<NewTerminalPayload>()
    .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::TerminalBootstrap,
        "service terminal bootstrap",
    )
    .await;
    fixture
        .client
        .terminal_send(
            &terminal.terminal_id,
            protocol::TerminalSendPayload {
                data: "exec python3 command.py\n".into(),
            },
        )
        .await
        .expect("execute real command");
    let (mut service, _) = tokio::select! {
        accepted = service_listener.accept() => accepted.expect("owned service control connection"),
        event = next_frame_matching_on(&mut fixture.client, "service startup outcome", |event| {
            event.stream == terminal.stream && matches!(event.kind, FrameKind::TerminalExit | FrameKind::TerminalError)
        }) => panic!("runtime exited before its required service connected: {:?}", event.kind),
    };
    let (mut descendant, _) = tokio::select! {
        accepted = descendant_listener.accept() => accepted.expect("owned descendant control connection"),
        event = next_frame_matching_on(&mut fixture.client, "descendant startup outcome", |event| {
            event.stream == terminal.stream && matches!(event.kind, FrameKind::TerminalExit | FrameKind::TerminalError)
        }) => panic!("runtime exited before its owned descendant connected: {:?}", event.kind),
    };
    assert_eq!(
        descendant
            .read_u8()
            .await
            .expect("foreground completed event"),
        b'P'
    );
    service
        .write_all(b"X")
        .await
        .expect("request actual required-service exit after foreground settled");
    let mut output = String::new();
    let exit = loop {
        let event = next_frame_matching_on(
            &mut fixture.client,
            "service failure terminal outcome",
            |e| {
                e.stream == terminal.stream
                    && matches!(
                        e.kind,
                        FrameKind::TerminalOutput
                            | FrameKind::TerminalExit
                            | FrameKind::TerminalError
                    )
            },
        )
        .await;
        match event.kind {
            FrameKind::TerminalOutput => output.push_str(
                &event
                    .parse_payload::<protocol::TerminalOutputPayload>()
                    .unwrap()
                    .data,
            ),
            FrameKind::TerminalExit => {
                break event
                    .parse_payload::<protocol::TerminalExitPayload>()
                    .unwrap();
            }
            _ => panic!("terminal transport failed instead of completing the runtime"),
        }
    };
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: project.id.clone(),
            force: true,
        })
        .await
        .expect("cleanup failed-runtime workbench");
    next_frame_matching_on(&mut fixture.client,"service workbench removed",|e|e.kind==FrameKind::ProjectNotify && matches!(e.parse_payload::<ProjectNotifyPayload>().unwrap(),ProjectNotifyPayload::Delete {project:p} if p.id==project.id)).await;
    assert!(!Path::new(&root.0).exists());
    assert_eq!(
        descendant
            .read_u8()
            .await
            .expect_err("owned descendant must close")
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    eprintln!(
        "SERVICE FAILURE boundary: exit_code={:?} typed_failure={} output_bytes={}",
        exit.exit_code,
        output.contains("\"reason\": \"runtime_service_failed\""),
        output.len()
    );
    assert_ne!(
        exit.exit_code,
        Some(0),
        "required service exit after foreground completion must fail the environment"
    );
    assert!(
        output.contains("\"reason\": \"runtime_service_failed\""),
        "actual terminal output must surface the typed runtime service failure"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bubblewrap_automatic_multiroot_builds_reuse_private_artifacts() {
    use protocol::{WorkbenchEngine, WorkbenchEnvironment, WorkbenchInitialization};

    async fn terminal_record(
        client: &mut client::Connection,
        terminal: &NewTerminalPayload,
        command: String,
        marker: &str,
    ) -> Value {
        client
            .terminal_send(
                &terminal.terminal_id,
                protocol::TerminalSendPayload { data: format!("{command}\nstatus=$?; printf '\\n{{\"terminal_command_finished\":%s}}\\n' \"$status\"\n") },
            )
            .await
            .unwrap();
        let mut output = String::new();
        loop {
            let event = next_frame_matching_on(client, "namespace command completion", |env| {
                env.stream == terminal.stream
                    && matches!(
                        env.kind,
                        FrameKind::TerminalOutput
                            | FrameKind::TerminalError
                            | FrameKind::TerminalExit
                    )
            })
            .await;
            assert_eq!(event.kind, FrameKind::TerminalOutput);
            output.push_str(
                &event
                    .parse_payload::<protocol::TerminalOutputPayload>()
                    .unwrap()
                    .data,
            );
            let records = output
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>();
            if let Some(completed) = records
                .iter()
                .find(|record| record.get("terminal_command_finished").is_some())
            {
                assert_eq!(
                    completed["terminal_command_finished"], 0,
                    "namespace command must succeed"
                );
                return records
                    .into_iter()
                    .find(|record| record.get(marker).is_some())
                    .expect("completed command must emit its assertion record");
            }
        }
    }

    let workspace = tempfile::tempdir().expect("sibling repositories");
    let rust = workspace.path().join("rust");
    let shared = workspace.path().join("shared");
    let web = workspace.path().join("web");
    let library = workspace.path().join("library");
    for root in [&rust, &shared, &web, &library] {
        fs::create_dir(root).unwrap();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "tests@example.com"]);
        git(root, &["config", "user.name", "Tests"]);
        fs::write(
            root.join(".gitignore"),
            "/target/\n/node_modules/\n/dist/\n",
        )
        .unwrap();
    }
    fs::create_dir(rust.join("src")).unwrap();
    fs::create_dir(shared.join("src")).unwrap();
    fs::write(rust.join("Cargo.toml"), "[package]\nname='warm_app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nwarm_shared={path='../shared'}\n").unwrap();
    fs::write(
        rust.join("src/main.rs"),
        "fn main() { println!(\"{}\", warm_shared::value()); }\n",
    )
    .unwrap();
    fs::write(
        shared.join("Cargo.toml"),
        "[package]\nname='warm_shared'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(shared.join("src/lib.rs"), "pub fn value() -> u32 { 42 }\n").unwrap();
    fs::write(web.join("package.json"), serde_json::to_vec(&json!({"name":"web","version":"1.0.0","dependencies":{"library":"file:../library"},"scripts":{"build":"node build.js"}})).unwrap()).unwrap();
    fs::write(web.join("package-lock.json"), serde_json::to_vec(&json!({"name":"web","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"web","version":"1.0.0","dependencies":{"library":"file:../library"}},"../library":{"name":"library","version":"1.0.0"},"node_modules/library":{"resolved":"../library","link":true}}})).unwrap()).unwrap();
    fs::write(web.join("build.js"), "const fs=require('fs'); const value=String(require('library')); fs.mkdirSync('dist',{recursive:true}); if(!fs.existsSync('dist/value') || fs.readFileSync('dist/value','utf8')!==value) fs.writeFileSync('dist/value',value);\n").unwrap();
    fs::write(
        library.join("package.json"),
        "{\"name\":\"library\",\"version\":\"1.0.0\",\"main\":\"index.js\"}\n",
    )
    .unwrap();
    fs::write(library.join("package-lock.json"), "{\"name\":\"library\",\"version\":\"1.0.0\",\"lockfileVersion\":3,\"requires\":true,\"packages\":{\"\":{\"name\":\"library\",\"version\":\"1.0.0\"}}}\n").unwrap();
    fs::write(library.join("index.js"), "module.exports=42;\n").unwrap();
    for root in [&rust, &shared, &web, &library] {
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "Ordinary project"]);
    }
    fs::create_dir(workspace.path().join(".cargo")).unwrap();
    fs::write(
        workspace.path().join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg=warm_ancestor','--check-cfg=cfg(warm_ancestor)']\n",
    )
    .unwrap();
    fs::create_dir(rust.join(".cargo")).unwrap();
    fs::write(
        rust.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg=warm_project','--check-cfg=cfg(warm_project)']\n",
    )
    .unwrap();
    fs::write(rust.join("src/main.rs"), "#[cfg(not(warm_ancestor))] compile_error!(\"ancestor flags missing\");\n#[cfg(not(warm_project))] compile_error!(\"project flags missing\");\nfn main() { println!(\"{}\", warm_shared::value()); }\n").unwrap();
    git(&rust, &["add", "."]);
    git(
        &rust,
        &[
            "commit",
            "-qm",
            "Require inherited and project Cargo settings",
        ],
    );
    let mut fixture = Fixture::new().await;
    let parent = create_project(&mut fixture.client, vec![&rust, &shared, &web, &library]).await;
    let native = create_workbench(&mut fixture.client, &parent, "native-before-selection").await;
    fixture
        .client
        .replace_setting(
            "/workbenches/engine",
            WorkbenchEngine::Bubblewrap,
            WorkbenchEngine::Native,
        )
        .await
        .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::SettingsWriteResult,
        "select Bubblewrap",
    )
    .await;
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("automatic-first".into()),
            name: "Automatic baseline".into(),
        })
        .await
        .unwrap();
    let preparation =
        next_frame_matching_on(&mut fixture.client, "visible baseline preparation", |env| {
            env.kind == FrameKind::CommandError
                || (env.kind == FrameKind::ProjectOperation
                    && matches!(
                        env.parse_payload::<protocol::ProjectOperationPayload>()
                            .unwrap()
                            .state,
                        protocol::ProjectOperationState::Running {
                            stage: protocol::ProjectOperationStage::PrepareBuilds
                        }
                    ))
        })
        .await;
    assert_eq!(
        preparation.kind,
        FrameKind::ProjectOperation,
        "multi-root setup must enter the typed preparation stage"
    );
    let outcome =
        next_frame_matching_on(&mut fixture.client, "automatic baseline outcome", |env| {
            matches!(env.kind, FrameKind::ProjectNotify | FrameKind::CommandError)
        })
        .await;
    if outcome.kind == FrameKind::CommandError {
        let error = outcome.parse_payload::<CommandErrorPayload>().unwrap();
        match &fixture.bootstrap.workbench_support.bubblewrap {
            protocol::WorkbenchAvailability::Unsupported { reason, .. } => {
                assert_eq!(error.workbench_reason, Some(*reason))
            }
            protocol::WorkbenchAvailability::Available => assert_eq!(
                error.workbench_reason,
                Some(protocol::WorkbenchUnsupportedReason::ReflinkUnsupported)
            ),
        }
        assert!(
            fixture
                .connect_with_bootstrap()
                .await
                .1
                .projects
                .iter()
                .all(|project| project.parent_project_id() != Some(&parent.id)
                    || project.id == native.id)
        );
        eprintln!(
            "Automatic warm flow explicitly unavailable: {:?}; no build/reuse pass claimed",
            error.workbench_reason
        );
        return;
    }
    let ProjectNotifyPayload::Upsert { project: first } = outcome.parse_payload().unwrap() else {
        panic!("created workbench")
    };
    let ProjectSource::GitWorkbench {
        environment:
            WorkbenchEnvironment::Bubblewrap {
                initialization: WorkbenchInitialization::Prepared { .. },
                additional_roots,
                build_blockers,
                ..
            },
        ..
    } = &first.source
    else {
        panic!("first creation must honestly report an initial build")
    };
    assert_eq!(additional_roots.len(), 3);
    assert!(build_blockers.is_empty());
    let second = create_workbench(&mut fixture.client, &parent, "automatic-second").await;
    assert!(
        matches!(
            second.source,
            ProjectSource::GitWorkbench {
                environment: WorkbenchEnvironment::Bubblewrap {
                    initialization: WorkbenchInitialization::Snapshot { .. },
                    ..
                },
                ..
            }
        ),
        "second creation must restore the verified composite cohort"
    );
    let first_roots = first.root_paths();
    let second_roots = second.root_paths();
    for roots in [&first_roots, &second_roots] {
        assert!(
            Path::new(&roots[0].0)
                .join("target/debug/warm_app")
                .is_file()
        );
        assert!(
            Path::new(&roots[1].0)
                .join("target/debug/libwarm_shared.rlib")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(Path::new(&roots[2].0).join("dist/value")).unwrap(),
            "42"
        );
        // npm's normal locked install may materialize a directory, not a
        // symlink. Assert the installed sibling code, not that storage choice.
        assert_eq!(
            fs::read(Path::new(&roots[2].0).join("node_modules/library/index.js")).unwrap(),
            fs::read(library.join("index.js")).unwrap()
        );
    }
    fixture
        .client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: second.id.clone(),
                root: second_roots[0].clone(),
                relative_cwd: None,
            },
            cols: 120,
            rows: 30,
        })
        .await
        .unwrap();
    let terminal = expect_kind(
        &mut fixture.client,
        FrameKind::NewTerminal,
        "warm ordinary build terminal",
    )
    .await
    .parse_payload::<NewTerminalPayload>()
    .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::TerminalBootstrap,
        "warm terminal bootstrap",
    )
    .await;
    let js_artifact = Path::new(&second_roots[2].0).join("dist/value");
    let js_mtime = fs::metadata(&js_artifact).unwrap().modified().unwrap();
    fixture.client.terminal_send(&terminal.terminal_id, protocol::TerminalSendPayload {data:r#"printf '\n'; cargo build --message-format=json && (cd ../web && npm run build); result=$?; printf '\n{"warm_flow_done":true,"status":%s}\n' "$result"
"#.into()}).await.unwrap();
    let mut output = String::new();
    loop {
        let event = next_frame_matching_on(
            &mut fixture.client,
            "ordinary warm build command completion",
            |env| {
                env.stream == terminal.stream
                    && matches!(
                        env.kind,
                        FrameKind::TerminalOutput
                            | FrameKind::TerminalError
                            | FrameKind::TerminalExit
                    )
            },
        )
        .await;
        assert_eq!(event.kind, FrameKind::TerminalOutput);
        output.push_str(
            &event
                .parse_payload::<protocol::TerminalOutputPayload>()
                .unwrap()
                .data,
        );
        if let Some(completion) = output
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["warm_flow_done"] == true)
        {
            assert_eq!(
                completion["status"], 0,
                "ordinary warm build command must succeed"
            );
            break;
        }
    }
    let artifacts = output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["reason"] == "compiler-artifact")
        .collect::<Vec<_>>();
    assert_eq!(
        artifacts.len(),
        2,
        "real Cargo must report both workspace artifacts"
    );
    assert!(
        artifacts.iter().all(|value| value["fresh"] == true),
        "Cargo must actually reuse the restored compilation outputs"
    );
    assert_eq!(
        fs::metadata(&js_artifact).unwrap().modified().unwrap(),
        js_mtime,
        "the normal sibling JS build must retain its already-built output"
    );
    let written = terminal_record(&mut fixture.client, &terminal, r#"python3 -c 'import json; from pathlib import Path; p=Path("../web/dist/value"); p.write_text("private artifact change"); Path("../shared/src/lib.rs").write_text("private source change"); print("\n"+json.dumps({"isolation_written":True,"inode":p.stat().st_ino}))'
"#.into(), "isolation_written").await;
    fixture
        .client
        .terminal_create(TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: first.id.clone(),
                root: first_roots[0].clone(),
                relative_cwd: None,
            },
            cols: 120,
            rows: 30,
        })
        .await
        .unwrap();
    let peer_terminal = expect_kind(
        &mut fixture.client,
        FrameKind::NewTerminal,
        "peer namespace terminal",
    )
    .await
    .parse_payload::<NewTerminalPayload>()
    .unwrap();
    expect_kind(
        &mut fixture.client,
        FrameKind::TerminalBootstrap,
        "peer terminal bootstrap",
    )
    .await;
    let checked = terminal_record(&mut fixture.client, &peer_terminal, format!(r#"python3 <<'PY'
import json
from pathlib import Path
p=Path("../web/dist/value")
print("\n"+json.dumps({{"isolation_checked":True,"artifact_unchanged":p.read_text()=="42","source_unchanged":Path("../shared/src/lib.rs").read_bytes()==Path({}).read_bytes(),"parent_unbuilt":not Path({}).exists() and not Path({}).exists(),"inode":p.stat().st_ino}}))
PY
"#, serde_json::to_string(&shared.join("src/lib.rs")).unwrap(), serde_json::to_string(&rust.join("target")).unwrap(), serde_json::to_string(&web.join("node_modules")).unwrap()), "isolation_checked").await;
    assert_eq!(
        checked["artifact_unchanged"], true,
        "peer namespace must retain its original artifact"
    );
    assert_eq!(
        checked["source_unchanged"], true,
        "peer and Native source must remain unchanged"
    );
    assert_eq!(
        checked["parent_unbuilt"], true,
        "preparation must not build or seed the Native parent"
    );
    assert_ne!(
        written["inode"], checked["inode"],
        "namespace artifacts must have independent private inodes"
    );
    fs::write(library.join("index.js"), "module.exports=43;\n").unwrap();
    git(&library, &["add", "."]);
    git(&library, &["commit", "-qm", "Change sibling build input"]);
    let changed = create_workbench(&mut fixture.client, &parent, "automatic-changed").await;
    assert!(
        matches!(
            changed.source,
            ProjectSource::GitWorkbench {
                environment: WorkbenchEnvironment::Bubblewrap {
                    initialization: WorkbenchInitialization::Prepared { .. },
                    ..
                },
                ..
            }
        ),
        "a changed sibling revision must prepare a new composite baseline"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&changed.root_paths()[2].0).join("dist/value")).unwrap(),
        "43"
    );
    let (_, replay) = fixture.connect_with_bootstrap().await;
    assert!(
        replay.projects.iter().any(|project| project == &native),
        "selecting Bubblewrap must not convert an existing Native workbench"
    );
    assert!(
        replay.projects.iter().any(|project| project == &second),
        "replay must retain the exact server-owned reuse state"
    );
    let external = workspace.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("assets"), "undeclared input").unwrap();
    std::os::unix::fs::symlink("../external", web.join("vendor")).unwrap();
    fs::write(web.join("build.js"), "const fs=require('fs'); fs.mkdirSync('dist'); fs.symlinkSync('../vendor/assets','dist/assets');\n").unwrap();
    git(&web, &["add", "."]);
    git(&web, &["commit", "-qm", "External chained artifact link"]);
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("reject-external-link".into()),
            name: "Rejected external link".into(),
        })
        .await
        .unwrap();
    let rejection = expect_command_error(
        &mut fixture.client,
        "reject external artifact symlink chain",
    )
    .await;
    assert_eq!(
        rejection.workbench_reason,
        Some(protocol::WorkbenchUnsupportedReason::IncompatibleEnvironment)
    );
    assert!(
        rejection.message.contains("symlink") && rejection.message.contains("outside registered")
    );
    fs::write(
        web.join("build.js"),
        "process.stderr.write('fixture compilation failed\\n'); process.exit(7);\n",
    )
    .unwrap();
    git(&web, &["add", "."]);
    git(&web, &["commit", "-qm", "Producer failure diagnostic"]);
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("reject-producer-failure".into()),
            name: "Rejected producer".into(),
        })
        .await
        .unwrap();
    let failure = expect_command_error(&mut fixture.client, "actionable producer failure").await;
    assert!(
        failure.message.contains("/mnt/tyde-workspace/web")
            && failure.message.contains("exit 7")
            && failure.message.contains("redacted build diagnostics")
    );
    let manifest = fs::read_to_string(rust.join("Cargo.toml"))
        .unwrap()
        .replace("../shared", shared.to_str().unwrap());
    fs::write(rust.join("Cargo.toml"), manifest).unwrap();
    git(&rust, &["add", "."]);
    git(
        &rust,
        &["commit", "-qm", "Unmapped absolute source dependency"],
    );
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("reject-native-source-alias".into()),
            name: "Rejected native source alias".into(),
        })
        .await
        .unwrap();
    let unmapped = expect_command_error(
        &mut fixture.client,
        "reject unmapped local source dependency",
    )
    .await;
    assert_eq!(
        unmapped.workbench_reason,
        Some(protocol::WorkbenchUnsupportedReason::RuntimeConfigurationInvalid)
    );
    assert!(
        unmapped.message.contains("local build input")
            && unmapped.message.contains("outside registered")
    );
    fs::write(rust.join("Cargo.toml"), format!("[package]\nname='warm_app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nwarm_shared='0.1.0'\n[patch.crates-io]\nwarm_shared={{path={}}}\n", serde_json::to_string(&shared).unwrap())).unwrap();
    git(&rust, &["add", "."]);
    git(
        &rust,
        &["commit", "-qm", "Unmapped absolute local Cargo patch"],
    );
    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("reject-native-patch-alias".into()),
            name: "Rejected native patch alias".into(),
        })
        .await
        .unwrap();
    let patch =
        expect_command_error(&mut fixture.client, "reject unmapped Cargo patch input").await;
    assert_eq!(
        patch.workbench_reason,
        Some(protocol::WorkbenchUnsupportedReason::RuntimeConfigurationInvalid)
    );
    assert!(
        patch.message.contains("local build input") && patch.message.contains("outside registered")
    );
    let (_, after_failure) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        after_failure
            .projects
            .iter()
            .filter(|project| project.parent_project_id() == Some(&parent.id))
            .count(),
        4,
        "failed setups may not register an environment or Native substitute"
    );
    for project in [first, second, changed, native] {
        fixture
            .client
            .workbench_remove(WorkbenchRemovePayload {
                id: project.id.clone(),
                force: true,
            })
            .await
            .unwrap();
        next_frame_matching_on(&mut fixture.client, "remove automatic build flow", |env| env.kind == FrameKind::ProjectNotify && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(), ProjectNotifyPayload::Delete {project:removed} if removed.id == project.id)).await;
        assert!(
            project
                .root_paths()
                .iter()
                .all(|root| !Path::new(&root.0).exists())
        );
    }
}
