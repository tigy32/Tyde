mod fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    BackendKind, CommandErrorCode, CommandErrorPayload, CustomAgent, CustomAgentId,
    CustomAgentUpsertPayload, Envelope, FrameKind, GitBranchName, HostSettingValue,
    NewAgentPayload, NewTerminalPayload, Project, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectDeleteRootPayload, ProjectNotifyPayload, ProjectRootPath,
    ProjectSource, SetSettingPayload, SpawnAgentParams, SpawnAgentPayload, Steering, SteeringId,
    SteeringScope, SteeringUpsertPayload, TeamCreatePayload, TeamMemberCreateSpec,
    TerminalCreatePayload, TerminalLaunchTarget, ToolPolicy, WorkbenchCreatePayload,
    WorkbenchRemovePayload,
};

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
    }
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectNotify);
    env.parse_payload()
        .expect("failed to parse ProjectNotifyPayload")
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
    match expect_project_notify(&mut fixture.client, "missing worktree remove").await {
        ProjectNotifyPayload::Delete { project } => assert_eq!(project.id, workbench.id),
        other => panic!("expected workbench delete, got {other:?}"),
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
