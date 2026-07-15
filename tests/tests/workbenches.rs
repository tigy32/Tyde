mod fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentId, AgentStartPayload, BackendAccessMode, BackendKind, CommandErrorCode,
    CommandErrorPayload, CustomAgent, CustomAgentId, CustomAgentUpsertPayload, Envelope, FrameKind,
    GitBranchName, HostSettingValue, NewAgentPayload, NewTerminalPayload, Project,
    ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload,
    ProjectNotifyPayload, ProjectRootPath, ProjectSource, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload, Steering, SteeringId, SteeringScope, SteeringUpsertPayload,
    TeamCreatePayload, TeamMemberCreateSpec, TerminalCreatePayload, TerminalLaunchTarget,
    ToolPolicy, WorkbenchCreatePayload, WorkbenchRemovePayload,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(10), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::BackendCapacity
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::SessionList
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
                | FrameKind::AgentActivityStats
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
        ) {
            continue;
        }
        return env;
    }
}

async fn expect_kind(client: &mut client::Connection, kind: FrameKind, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(10), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if env.kind == kind {
            return env;
        }
        if env.kind == FrameKind::AgentBootstrap && kind == FrameKind::AgentStart {
            let bootstrap: protocol::AgentBootstrapPayload =
                env.parse_payload().expect("AgentBootstrap payload");
            if let Some(protocol::AgentBootstrapEvent::AgentStart(start)) = bootstrap
                .events
                .into_iter()
                .find(|event| matches!(event, protocol::AgentBootstrapEvent::AgentStart(_)))
            {
                return Envelope::from_payload(env.stream, FrameKind::AgentStart, env.seq, &start)
                    .expect("serialize AgentStart");
            }
        }
    }
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

async fn wait_for_command_error(
    client: &mut client::Connection,
    context: &str,
    duration: Duration,
) -> Option<CommandErrorPayload> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
        let env = match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed while waiting for {context}"),
            Ok(Err(err)) => panic!("next_event failed while waiting for {context}: {err:?}"),
            Err(_) => return None,
        };
        if env.kind == FrameKind::CommandError {
            return Some(
                env.parse_payload()
                    .expect("failed to parse CommandErrorPayload"),
            );
        }
    }
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
    let url = fixture.agent_control_http_url().await;
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
    loop {
        let payload: NewAgentPayload = expect_kind(client, FrameKind::NewAgent, name)
            .await
            .parse_payload()
            .expect("NewAgent");
        if payload.name == name {
            return payload;
        }
    }
}

#[tokio::test]
async fn workbench_create_and_remove_round_trip_real_git_repo() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("round-trip");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;

    let workbench = create_workbench(&mut fixture.client, &parent, "feature/login").await;
    let expected_path = expected_worktree_path(repo.path(), "feature%2Flogin");

    match &workbench.source {
        ProjectSource::GitWorkbench {
            parent_project_id,
            branch,
            roots,
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

    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
        })
        .await
        .expect("workbench_remove write failed");
    match expect_project_notify(&mut fixture.client, "workbench remove").await {
        ProjectNotifyPayload::Delete { project } => assert_eq!(project, workbench),
        other => panic!("expected workbench delete, got {other:?}"),
    }
    assert!(!expected_path.exists());
}

#[tokio::test]
async fn workbench_create_rejects_path_collision_and_existing_branch() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("create-failures");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;

    let collision_path = expected_worktree_path(repo.path(), "feature%2Fcollision");
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
    assert_eq!(error.operation, "workbench_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("already exists"));

    let registered_path = expected_worktree_path(repo.path(), "feature%2Fregistered");
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
async fn workbench_remove_rejects_dirty_worktree() {
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
        })
        .await
        .expect("workbench_remove write failed");
    let error = expect_command_error(&mut fixture.client, "dirty worktree remove").await;
    assert_eq!(error.operation, "workbench_remove");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(
        error
            .message
            .contains(worktree_root.to_string_lossy().as_ref())
    );
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

    let (error_a, error_b) = tokio::join!(
        wait_for_command_error(
            &mut client_a,
            "client A concurrent workbench_create",
            Duration::from_secs(30),
        ),
        wait_for_command_error(
            &mut client_b,
            "client B concurrent workbench_create",
            Duration::from_secs(30),
        ),
    );
    let errors = [error_a, error_b].into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "exactly one concurrent create should fail");
    assert_eq!(errors[0].operation, "workbench_create");
    assert_eq!(errors[0].code, CommandErrorCode::Conflict);
    assert!(
        errors[0].message.contains("already exists"),
        "unexpected error message: {}",
        errors[0].message
    );
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
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("missing-worktree");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let workbench = create_workbench(&mut fixture.client, &parent, "missing-worktree").await;
    let worktree_root = PathBuf::from(project_roots(&workbench)[0].clone());
    fs::remove_dir_all(&worktree_root).expect("remove worktree dir out of band");

    // A worktree dir deleted out of band (manual `rm -rf`, or a half-failed
    // earlier removal) must not brick the record: removal succeeds, prunes
    // git's worktree bookkeeping, and deletes the store record.
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
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
                    ProjectNotifyPayload::Delete { project } if project.id == workbench.id => break,
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
                assert!(error.fatal, "watcher error must be fatal");
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
}

#[tokio::test]
async fn workbench_remove_rejects_team_member_reference() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("team-blocker");
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let workbench = create_workbench(&mut fixture.client, &parent, "team-blocker").await;

    fixture
        .client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
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

    // Removing a workbench referenced by a team member would persist a
    // dangling ProjectId in agent_teams.json and brick the next boot.
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: workbench.id.clone(),
        })
        .await
        .expect("workbench_remove write failed");
    let error: CommandErrorPayload = expect_kind(
        &mut fixture.client,
        FrameKind::CommandError,
        "team member blocker",
    )
    .await
    .parse_payload()
    .expect("parse CommandError");
    assert_eq!(error.operation, "workbench_remove");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(
        error.message.contains("team member"),
        "unexpected error message: {}",
        error.message
    );
}

#[tokio::test]
async fn workbench_remove_rejects_live_agent_live_terminal_session_and_steering_blockers() {
    let mut fixture = Fixture::new().await;

    let agent_repo = init_git_repo("agent-blocker");
    let agent_parent = create_project(&mut fixture.client, vec![agent_repo.path()]).await;
    let agent_workbench =
        create_workbench(&mut fixture.client, &agent_parent, "agent-blocker").await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("workbench-live-agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(agent_workbench.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(&agent_workbench),
                prompt: "__mock_slow__ hold workbench".to_owned(),
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
    // No AgentStart wait is needed before attempting removal: the agent is
    // inserted into the host registry (with its project_id) synchronously
    // under the host state lock before the NewAgent frame is emitted, so
    // receiving NewAgent guarantees the live-agent blocker sees it.
    let _ = expect_kind(
        &mut fixture.client,
        FrameKind::NewAgent,
        "live agent NewAgent",
    )
    .await;
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: agent_workbench.id.clone(),
        })
        .await
        .expect("workbench_remove write failed");
    let error = expect_command_error(&mut fixture.client, "live agent blocker").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("agent"));

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
        })
        .await
        .expect("workbench_remove write failed");
    let error = expect_command_error(&mut fixture.client, "live terminal blocker").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("terminal"));

    let session_repo = init_git_repo("session-blocker");
    let session_parent = create_project(&mut fixture.client, vec![session_repo.path()]).await;
    let session_workbench =
        create_workbench(&mut fixture.client, &session_parent, "session-blocker").await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
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
    // No AgentStart wait is needed here either (registration is synchronous
    // before the NewAgent echo); the session blocker additionally relies on
    // the ChatEvent + AgentClosed waits below, which guarantee the session
    // record is persisted before removal is attempted.
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
    fixture
        .client
        .workbench_remove(WorkbenchRemovePayload {
            id: session_workbench.id.clone(),
        })
        .await
        .expect("workbench_remove write failed");
    let error = expect_command_error(&mut fixture.client, "persisted session blocker").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("session"));

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
        })
        .await
        .expect("workbench_remove write failed");
    let error = expect_command_error(&mut fixture.client, "steering blocker").await;
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(error.message.contains("steering"));
}

#[tokio::test]
async fn agent_control_creates_lists_and_spawns_workbenches() {
    let repo = init_git_repo("agent-control-workbench");
    let mut fixture = Fixture::new().await;
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
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
    let orchestrator = loop {
        let env = expect_kind(&mut fixture.client, FrameKind::NewAgent, "orchestrator").await;
        let agent: NewAgentPayload = env.parse_payload().expect("NewAgent");
        if agent.name == "workbench-orchestrator" {
            break agent;
        }
    };

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
}

#[tokio::test]
async fn agent_control_workbenches_enforce_auth_access_and_scope() {
    let repo = init_git_repo("scope-parent");
    let other_repo = init_git_repo("scope-other");
    let mut fixture = Fixture::new().await;
    let parent = create_project(&mut fixture.client, vec![repo.path()]).await;
    let other = create_project(&mut fixture.client, vec![other_repo.path()]).await;

    for (tool, arguments) in [
        ("tyde_list_workbenches", json!({})),
        (
            "tyde_create_workbench",
            json!({
                "parent_project_id": parent.id.0, "branch": "feature/no-auth"
            }),
        ),
    ] {
        let (is_error, body) = call_agent_control_optional(&fixture, None, tool, arguments).await;
        assert!(is_error, "unauthenticated {tool} succeeded: {body}");
    }
    assert!(!expected_worktree_path(repo.path(), "feature%2Fno-auth").exists());

    let read_only = spawn_project_caller(
        &mut fixture.client,
        &parent,
        "read-only-workbench-caller",
        BackendAccessMode::ReadOnly,
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
    assert!(is_error, "read-only create succeeded: {body}");
    assert!(body.contains("ReadOnly"));
    assert!(!expected_worktree_path(repo.path(), "feature%2Fread-only").exists());

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
    assert!(!expected_worktree_path(other_repo.path(), "feature%2Fout-of-scope").exists());
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
        assert!(!expected_worktree_path(root, "feature%2Fmulti-fail").exists());
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
        })
        .await
        .expect("send concurrent remove");
    hook.wait_until_reached().await;

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
    hook.resume();

    let (is_error, body) = spawn_task.await.expect("spawn task join");
    assert!(is_error, "remove and spawn both succeeded: {body}");
    assert!(
        body.contains("missing project") || body.contains("being removed"),
        "spawn returned a success-shaped failure: {body}"
    );
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
