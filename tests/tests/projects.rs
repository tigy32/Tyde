mod fixture;

use fixture::Fixture;
use protocol::{
    CommandErrorCode, CommandErrorPayload, DiffContextMode, Envelope, FileEntryOp, FrameKind,
    Project, ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectDiffScope,
    ProjectFileContentsPayload, ProjectFileListPayload, ProjectGitDiffLineKind,
    ProjectGitDiffPayload, ProjectGitStatusPayload, ProjectListDirPayload, ProjectNotifyPayload,
    ProjectPath, ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRenamePayload,
    ProjectReorderPayload, ProjectRootPath, ProjectStageFilePayload, ProjectStageHunkPayload,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    loop {
        match tokio::time::timeout(duration, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Ok(Some(env)))
                if matches!(
                    env.kind,
                    FrameKind::HostSettings
                        | FrameKind::SessionSchemas
                        | FrameKind::BackendSetup
                        | FrameKind::QueuedMessages
                        | FrameKind::SessionSettings
                        | FrameKind::SessionList
                ) =>
            {
                continue;
            }
            Ok(Ok(Some(env))) => panic!(
                "unexpected event before {context}: kind={} stream={}",
                env.kind, env.stream
            ),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
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

async fn expect_project_file_list(
    client: &mut client::Connection,
    context: &str,
) -> ProjectFileListPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectFileList);
    env.parse_payload()
        .expect("failed to parse ProjectFileListPayload")
}

async fn expect_project_git_status(
    client: &mut client::Connection,
    context: &str,
) -> ProjectGitStatusPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectGitStatus);
    env.parse_payload()
        .expect("failed to parse ProjectGitStatusPayload")
}

async fn expect_project_file_contents(
    client: &mut client::Connection,
    context: &str,
) -> ProjectFileContentsPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectFileContents);
    env.parse_payload()
        .expect("failed to parse ProjectFileContentsPayload")
}

async fn expect_project_git_diff(
    client: &mut client::Connection,
    context: &str,
) -> ProjectGitDiffPayload {
    let env = expect_next_event(client, context).await;
    assert_eq!(env.kind, FrameKind::ProjectGitDiff);
    env.parse_payload()
        .expect("failed to parse ProjectGitDiffPayload")
}

async fn request_project_diff(
    client: &mut client::Connection,
    project_id: &protocol::ProjectId,
    payload: ProjectReadDiffPayload,
    context: &str,
) -> ProjectGitDiffPayload {
    client
        .project_read_diff(project_id, payload)
        .await
        .expect("project_read_diff failed");
    expect_project_git_diff(client, context).await
}

async fn create_project(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots,
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

/// Create a project with real filesystem roots.
/// Drains the server-pushed initial file list + git status.
async fn create_project_with_real_roots(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    let project = create_project(client, name, roots).await;
    let _ = expect_project_file_list(client, "initial server-pushed file list").await;
    let _ = expect_project_git_status(client, "initial server-pushed git status").await;
    project
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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|err| {
            panic!("failed to create parent '{}': {}", parent.display(), err)
        });
    }
    fs::write(path, contents)
        .unwrap_or_else(|err| panic!("failed to write '{}': {}", path.display(), err));
}

fn init_git_repo(name: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let repo =
        tempfile::tempdir().unwrap_or_else(|err| panic!("failed to create tempdir: {}", err));
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "tests@example.com"]);
    git(repo.path(), &["config", "user.name", "Tests"]);
    for (relative_path, contents) in files {
        write_file(&repo.path().join(relative_path), contents);
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", name]);
    repo
}

#[tokio::test]
async fn create_project_emits_upsert() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Tyde".to_owned(),
            roots: vec!["/tmp/tyde".to_owned()],
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(&mut fixture.client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => {
            assert!(!project.id.0.is_empty());
            assert_eq!(project.name, "Tyde");
            assert_eq!(project.roots, vec!["/tmp/tyde".to_owned()]);
        }
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

#[tokio::test]
async fn create_project_notifies_all_connected_clients() {
    let mut fixture = Fixture::new().await;
    let mut client2 = fixture.connect().await;
    let mut client3 = fixture.connect().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Shared".to_owned(),
            roots: vec!["/tmp/shared".to_owned()],
        })
        .await
        .expect("project_create failed");

    let mut delivered = Vec::new();
    for client in [&mut fixture.client, &mut client2, &mut client3] {
        match expect_project_notify(client, "shared project notify").await {
            ProjectNotifyPayload::Upsert { project } => delivered.push(project),
            other => panic!("expected upsert project notification, got {other:?}"),
        }
    }

    assert_eq!(delivered.len(), 3);
    assert_eq!(delivered[0], delivered[1]);
    assert_eq!(delivered[1], delivered[2]);
}

#[tokio::test]
async fn late_joining_client_gets_all_existing_projects() {
    let mut fixture = Fixture::new().await;

    let project_a =
        create_project(&mut fixture.client, "Alpha", vec!["/tmp/alpha".to_owned()]).await;
    let project_b = create_project(&mut fixture.client, "Beta", vec!["/tmp/beta".to_owned()]).await;
    let project_c =
        create_project(&mut fixture.client, "Gamma", vec!["/tmp/gamma".to_owned()]).await;

    let mut late_client = fixture.connect().await;

    let replayed = vec![
        expect_project_notify(&mut late_client, "project replay 1").await,
        expect_project_notify(&mut late_client, "project replay 2").await,
        expect_project_notify(&mut late_client, "project replay 3").await,
    ]
    .into_iter()
    .map(|payload| match payload {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected replayed upsert project notification, got {other:?}"),
    })
    .collect::<Vec<_>>();

    assert_eq!(replayed, vec![project_a, project_b, project_c]);
}

#[tokio::test]
async fn rename_project_emits_updated_upsert() {
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        "Original",
        vec!["/tmp/original".to_owned()],
    )
    .await;

    fixture
        .client
        .project_rename(ProjectRenamePayload {
            id: project.id.clone(),
            name: "Renamed".to_owned(),
        })
        .await
        .expect("project_rename failed");

    match expect_project_notify(&mut fixture.client, "project rename").await {
        ProjectNotifyPayload::Upsert { project: renamed } => {
            assert_eq!(renamed.id, project.id);
            assert_eq!(renamed.name, "Renamed");
            assert_eq!(renamed.roots, vec!["/tmp/original".to_owned()]);
        }
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

#[tokio::test]
async fn reorder_projects_persists_and_replays_in_custom_order() {
    let mut fixture = Fixture::new().await;
    let project_a =
        create_project(&mut fixture.client, "Alpha", vec!["/tmp/alpha".to_owned()]).await;
    let project_b = create_project(&mut fixture.client, "Beta", vec!["/tmp/beta".to_owned()]).await;
    let project_c =
        create_project(&mut fixture.client, "Gamma", vec!["/tmp/gamma".to_owned()]).await;

    fixture
        .client
        .project_reorder(ProjectReorderPayload {
            project_ids: vec![
                project_c.id.clone(),
                project_a.id.clone(),
                project_b.id.clone(),
            ],
        })
        .await
        .expect("project_reorder failed");

    let reordered = vec![
        expect_project_notify(&mut fixture.client, "project reorder 1").await,
        expect_project_notify(&mut fixture.client, "project reorder 2").await,
        expect_project_notify(&mut fixture.client, "project reorder 3").await,
    ]
    .into_iter()
    .map(|payload| match payload {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected reordered upsert project notification, got {other:?}"),
    })
    .collect::<Vec<_>>();

    let expected = vec![
        Project {
            sort_order: 0,
            ..project_c.clone()
        },
        Project {
            sort_order: 1,
            ..project_a.clone()
        },
        Project {
            sort_order: 2,
            ..project_b.clone()
        },
    ];
    assert_eq!(reordered, expected);

    let mut fresh_client = fixture.connect_fresh_host().await;
    let replayed = vec![
        expect_project_notify(&mut fresh_client, "reordered replay 1").await,
        expect_project_notify(&mut fresh_client, "reordered replay 2").await,
        expect_project_notify(&mut fresh_client, "reordered replay 3").await,
    ]
    .into_iter()
    .map(|payload| match payload {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected reordered replay upsert project notification, got {other:?}"),
    })
    .collect::<Vec<_>>();

    assert_eq!(replayed, expected);
}

#[tokio::test]
async fn add_root_emits_updated_upsert() {
    let mut fixture = Fixture::new().await;
    let project =
        create_project(&mut fixture.client, "Roots", vec!["/tmp/root-a".to_owned()]).await;

    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: project.id.clone(),
            root: "/tmp/root-b".to_owned(),
        })
        .await
        .expect("project_add_root failed");

    match expect_project_notify(&mut fixture.client, "project add root").await {
        ProjectNotifyPayload::Upsert { project: updated } => {
            assert_eq!(updated.id, project.id);
            assert_eq!(
                updated.roots,
                vec!["/tmp/root-a".to_owned(), "/tmp/root-b".to_owned()]
            );
        }
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

#[tokio::test]
async fn delete_project_emits_delete_and_removes_it_from_replay() {
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        "Delete Me",
        vec!["/tmp/delete-me".to_owned()],
    )
    .await;

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: project.id.clone(),
        })
        .await
        .expect("project_delete failed");

    match expect_project_notify(&mut fixture.client, "project delete").await {
        ProjectNotifyPayload::Delete { project: deleted } => assert_eq!(deleted, project),
        other => panic!("expected delete project notification, got {other:?}"),
    }

    let mut late_client = fixture.connect().await;
    expect_no_event(
        &mut late_client,
        Duration::from_millis(150),
        "deleted project should not replay",
    )
    .await;
}

#[tokio::test]
async fn projects_persist_and_replay_from_fresh_host() {
    let mut fixture = Fixture::new().await;

    let project_a = create_project(
        &mut fixture.client,
        "Persist A",
        vec!["/tmp/persist-a".to_owned()],
    )
    .await;
    let project_b = create_project(
        &mut fixture.client,
        "Persist B",
        vec![
            "/tmp/persist-b".to_owned(),
            "/tmp/persist-b-extra".to_owned(),
        ],
    )
    .await;

    let mut fresh_client = fixture.connect_fresh_host().await;
    let replayed = vec![
        expect_project_notify(&mut fresh_client, "persisted replay 1").await,
        expect_project_notify(&mut fresh_client, "persisted replay 2").await,
    ]
    .into_iter()
    .map(|payload| match payload {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected persisted upsert project notification, got {other:?}"),
    })
    .collect::<Vec<_>>();

    assert_eq!(replayed, vec![project_a, project_b]);
}

#[tokio::test]
async fn invalid_project_create_surfaces_command_error_and_keeps_connection_alive() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Invalid".to_owned(),
            roots: vec!["/tmp/dup".to_owned(), "/tmp/dup".to_owned()],
        })
        .await
        .expect("project_create write failed");

    let error = expect_command_error(&mut fixture.client, "command error").await;
    assert_eq!(error.operation, "project_create");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(!error.fatal);
    assert!(
        error.message.contains("roots must be unique"),
        "unexpected project_create error: {}",
        error.message
    );

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(150),
        "connection should stay open after invalid project_create",
    )
    .await;
}

#[tokio::test]
async fn late_joining_client_gets_server_pushed_project_state() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("late-push", &[("src/lib.rs", "pub fn a() {}\n")]);

    let project = create_project(
        &mut fixture.client,
        "Late Push",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let mut late_client = fixture.connect().await;

    // Late client should get the project notify replay
    match expect_project_notify(&mut late_client, "late project replay").await {
        ProjectNotifyPayload::Upsert { project: replayed } => {
            assert_eq!(replayed.id, project.id);
        }
        other => panic!("expected upsert, got {other:?}"),
    }

    // Project state is server-pushed on subscription replay; the frontend does
    // not request a refresh.
    let file_list =
        expect_project_file_list(&mut late_client, "late server-pushed file list").await;
    assert_eq!(file_list.roots.len(), 1);
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );

    let git_status =
        expect_project_git_status(&mut late_client, "late server-pushed git status").await;
    assert_eq!(git_status.roots.len(), 1);
    assert!(git_status.roots[0].clean);
}

#[tokio::test]
async fn create_project_pushes_file_list_and_git_status() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("server-push", &[("src/lib.rs", "pub fn a() {}\n")]);

    let project = create_project(
        &mut fixture.client,
        "Server Push",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    // Server owns project state and pushes it after the project is created.
    let file_list =
        expect_project_file_list(&mut fixture.client, "server-pushed file list after create").await;
    assert_eq!(file_list.roots.len(), 1);
    assert_eq!(file_list.roots[0].root.0, project.roots[0]);
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );

    let git_status =
        expect_project_git_status(&mut fixture.client, "server-pushed git status after create")
            .await;
    assert_eq!(git_status.roots.len(), 1);
    assert!(git_status.roots[0].clean);
}

#[tokio::test]
async fn project_create_pushes_file_list_and_git_status_for_all_roots() {
    let mut fixture = Fixture::new().await;
    let repo_a = init_git_repo("repo-a", &[("src/lib.rs", "pub fn a() {}\n")]);
    let repo_b = init_git_repo("repo-b", &[("app/main.rs", "fn main() {}\n")]);

    let project = create_project(
        &mut fixture.client,
        "Multi Root",
        vec![
            repo_a.path().to_string_lossy().to_string(),
            repo_b.path().to_string_lossy().to_string(),
        ],
    )
    .await;

    let file_list =
        expect_project_file_list(&mut fixture.client, "server-pushed project file list").await;
    assert_eq!(file_list.roots.len(), 2);
    assert_eq!(file_list.roots[0].root.0, project.roots[0]);
    assert_eq!(file_list.roots[1].root.0, project.roots[1]);
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );
    assert!(
        file_list.roots[1]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "app/main.rs")
    );

    let git_status = expect_project_git_status(&mut fixture.client, "project git status").await;
    assert_eq!(git_status.roots.len(), 2);
    assert!(git_status.roots.iter().all(|root| root.clean));
    assert!(git_status.roots.iter().all(|root| root.files.is_empty()));
}

#[tokio::test]
async fn project_read_file_returns_file_contents() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-file",
        &[("src/main.rs", "fn main() { println!(\"hi\"); }\n")],
    );
    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read File",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project.roots[0].clone()),
                    relative_path: "src/main.rs".to_owned(),
                },
            },
        )
        .await
        .expect("project_read_file failed");

    let file_contents =
        expect_project_file_contents(&mut fixture.client, "project file contents").await;
    assert_eq!(file_contents.path.relative_path, "src/main.rs");
    assert!(!file_contents.is_binary);
    assert_eq!(
        file_contents.contents.as_deref(),
        Some("fn main() { println!(\"hi\"); }\n")
    );
}

#[tokio::test]
async fn project_read_file_accepts_absolute_path_with_line_suffix() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-file-absolute-link",
        &[("src/main.rs", "fn main() { println!(\"hi\"); }\n")],
    );
    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read File Absolute Link",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project.roots[0].clone()),
                    relative_path: format!("{}/src/main.rs:366", project.roots[0]),
                },
            },
        )
        .await
        .expect("project_read_file failed");

    let file_contents = expect_project_file_contents(
        &mut fixture.client,
        "project file contents for absolute file link",
    )
    .await;
    assert_eq!(file_contents.path.relative_path, "src/main.rs");
    assert!(!file_contents.is_binary);
    assert_eq!(
        file_contents.contents.as_deref(),
        Some("fn main() { println!(\"hi\"); }\n")
    );
}

#[tokio::test]
async fn project_read_file_outside_project_does_not_crash_host() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-file-invalid-absolute-link",
        &[("src/main.rs", "fn main() { println!(\"hi\"); }\n")],
    );
    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read File Invalid Absolute Link",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project.roots[0].clone()),
                    relative_path: "/tmp/not-in-project.rs:12".to_owned(),
                },
            },
        )
        .await
        .expect("project_read_file failed");

    let error = expect_command_error(&mut fixture.client, "invalid absolute project read").await;
    assert_eq!(error.operation, "project_read_file");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("project"),
        "unexpected invalid absolute project read error: {}",
        error.message
    );

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project.roots[0].clone()),
                    relative_path: "src/main.rs".to_owned(),
                },
            },
        )
        .await
        .expect("project_read_file failed after invalid absolute link");

    let file_contents = expect_project_file_contents(
        &mut fixture.client,
        "project file contents after invalid absolute file link",
    )
    .await;
    assert_eq!(file_contents.path.relative_path, "src/main.rs");
    assert!(!file_contents.is_binary);
}

#[tokio::test]
async fn project_read_diff_returns_unstaged_diff() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-diff",
        &[("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")],
    );
    write_file(
        &repo.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "project git diff",
    )
    .await;
    assert_eq!(diff.scope, ProjectDiffScope::Unstaged);
    assert_eq!(diff.context_mode, DiffContextMode::Hunks);
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].relative_path, "src/main.rs");
    assert_eq!(diff.files[0].hunks.len(), 1);
    assert!(
        diff.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.text.contains("println!(\"hello\")"))
    );
}

#[tokio::test]
async fn project_read_diff_untracked_file_appears_as_all_added() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("read-diff-untracked", &[("src/main.rs", "fn main() {}\n")]);
    write_file(&repo.path().join("src/new.rs"), "alpha\nbeta\ngamma\n");

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff Untracked",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/new.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "untracked file diff",
    )
    .await;

    assert_eq!(diff.scope, ProjectDiffScope::Unstaged);
    assert_eq!(diff.files.len(), 1);
    let file = &diff.files[0];
    assert_eq!(file.relative_path, "src/new.rs");
    assert_eq!(file.hunks.len(), 1);

    let hunk = &file.hunks[0];
    assert_eq!(hunk.old_start, 0);
    assert_eq!(hunk.old_count, 0);
    assert_eq!(hunk.new_start, 1);
    assert_eq!(hunk.new_count, 3);
    assert_eq!(hunk.lines.len(), 3);
    let expected = ["alpha", "beta", "gamma"];
    for (index, line) in hunk.lines.iter().enumerate() {
        assert_eq!(line.kind, ProjectGitDiffLineKind::Added);
        assert_eq!(line.text, expected[index]);
        assert_eq!(line.old_line_number, None);
        assert_eq!(line.new_line_number, Some((index + 1) as u32));
    }
}

#[tokio::test]
async fn project_read_diff_unstaged_includes_both_modified_and_untracked() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-diff-mixed",
        &[("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")],
    );
    write_file(
        &repo.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    );
    write_file(&repo.path().join("src/new.rs"), "pub fn added() {}\n");

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff Mixed",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
            context_mode: DiffContextMode::Hunks,
        },
        "mixed unstaged diff",
    )
    .await;

    assert_eq!(diff.scope, ProjectDiffScope::Unstaged);
    assert_eq!(diff.context_mode, DiffContextMode::Hunks);
    let paths: Vec<&str> = diff
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect();
    assert_eq!(paths, vec!["src/main.rs", "src/new.rs"]);

    let modified = diff
        .files
        .iter()
        .find(|file| file.relative_path == "src/main.rs")
        .expect("missing modified tracked file");
    assert_eq!(modified.hunks.len(), 1);
    let modified_hunk = &modified.hunks[0];
    assert_eq!(modified_hunk.old_start, 1);
    assert_eq!(modified_hunk.old_count, 3);
    assert_eq!(modified_hunk.new_start, 1);
    assert_eq!(modified_hunk.new_count, 3);
    assert_eq!(modified_hunk.lines.len(), 4);
    assert_eq!(modified_hunk.lines[0].kind, ProjectGitDiffLineKind::Context);
    assert_eq!(modified_hunk.lines[0].text, "fn main() {");
    assert_eq!(modified_hunk.lines[0].old_line_number, Some(1));
    assert_eq!(modified_hunk.lines[0].new_line_number, Some(1));
    assert_eq!(modified_hunk.lines[1].kind, ProjectGitDiffLineKind::Removed);
    assert_eq!(modified_hunk.lines[1].text, "    println!(\"hi\");");
    assert_eq!(modified_hunk.lines[1].old_line_number, Some(2));
    assert_eq!(modified_hunk.lines[1].new_line_number, None);
    assert_eq!(modified_hunk.lines[2].kind, ProjectGitDiffLineKind::Added);
    assert_eq!(modified_hunk.lines[2].text, "    println!(\"hello\");");
    assert_eq!(modified_hunk.lines[2].old_line_number, None);
    assert_eq!(modified_hunk.lines[2].new_line_number, Some(2));
    assert_eq!(modified_hunk.lines[3].kind, ProjectGitDiffLineKind::Context);
    assert_eq!(modified_hunk.lines[3].text, "}");
    assert_eq!(modified_hunk.lines[3].old_line_number, Some(3));
    assert_eq!(modified_hunk.lines[3].new_line_number, Some(3));

    let untracked = diff
        .files
        .iter()
        .find(|file| file.relative_path == "src/new.rs")
        .expect("missing untracked file");
    assert_eq!(untracked.hunks.len(), 1);
    let untracked_hunk = &untracked.hunks[0];
    assert_eq!(untracked_hunk.old_start, 0);
    assert_eq!(untracked_hunk.old_count, 0);
    assert_eq!(untracked_hunk.new_start, 1);
    assert_eq!(untracked_hunk.new_count, 1);
    assert_eq!(untracked_hunk.lines.len(), 1);
    assert_eq!(untracked_hunk.lines[0].kind, ProjectGitDiffLineKind::Added);
    assert_eq!(untracked_hunk.lines[0].text, "pub fn added() {}");
    assert_eq!(untracked_hunk.lines[0].old_line_number, None);
    assert_eq!(untracked_hunk.lines[0].new_line_number, Some(1));

    let filtered_untracked = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/new.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "filtered untracked diff",
    )
    .await;

    assert_eq!(filtered_untracked.scope, ProjectDiffScope::Unstaged);
    assert_eq!(filtered_untracked.context_mode, DiffContextMode::Hunks);
    assert_eq!(filtered_untracked.files.len(), 1);
    let filtered_file = &filtered_untracked.files[0];
    assert_eq!(filtered_file.relative_path, "src/new.rs");
    assert_eq!(filtered_file.hunks.len(), 1);
    let filtered_hunk = &filtered_file.hunks[0];
    assert_eq!(filtered_hunk.old_start, 0);
    assert_eq!(filtered_hunk.old_count, 0);
    assert_eq!(filtered_hunk.new_start, 1);
    assert_eq!(filtered_hunk.new_count, 1);
    assert_eq!(filtered_hunk.lines.len(), 1);
    assert_eq!(filtered_hunk.lines[0].kind, ProjectGitDiffLineKind::Added);
    assert_eq!(filtered_hunk.lines[0].text, "pub fn added() {}");
    assert_eq!(filtered_hunk.lines[0].old_line_number, None);
    assert_eq!(filtered_hunk.lines[0].new_line_number, Some(1));
}

#[tokio::test]
async fn project_read_diff_staged_scope_excludes_untracked() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("read-diff-staged", &[("src/main.rs", "fn main() {}\n")]);
    write_file(&repo.path().join("src/new.rs"), "pub fn added() {}\n");

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff Staged",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Staged,
            path: None,
            context_mode: DiffContextMode::Hunks,
        },
        "staged diff excludes untracked",
    )
    .await;

    assert_eq!(diff.scope, ProjectDiffScope::Staged);
    assert!(diff.files.is_empty());
}

#[tokio::test]
async fn project_read_diff_hunks_mode_returns_typed_line_numbers() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-diff-hunks-mode",
        &[(
            "src/main.rs",
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\n",
        )],
    );
    write_file(
        &repo.path().join("src/main.rs"),
        "line1\nline2\nline2 inserted\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline11 inserted\nline12\n",
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff Hunks Mode",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "hunks mode diff",
    )
    .await;

    assert_eq!(diff.context_mode, DiffContextMode::Hunks);
    assert_eq!(diff.files.len(), 1);
    let file = &diff.files[0];
    assert_eq!(file.hunks.len(), 2);

    let first_hunk = &file.hunks[0];
    assert_eq!(first_hunk.old_start, 1);
    assert_eq!(first_hunk.old_count, 5);
    assert_eq!(first_hunk.new_start, 1);
    assert_eq!(first_hunk.new_count, 6);
    assert_eq!(first_hunk.lines.len(), 6);
    assert_eq!(first_hunk.lines[0].kind, ProjectGitDiffLineKind::Context);
    assert_eq!(first_hunk.lines[0].text, "line1");
    assert_eq!(first_hunk.lines[0].old_line_number, Some(1));
    assert_eq!(first_hunk.lines[0].new_line_number, Some(1));
    assert_eq!(first_hunk.lines[1].kind, ProjectGitDiffLineKind::Context);
    assert_eq!(first_hunk.lines[1].text, "line2");
    assert_eq!(first_hunk.lines[1].old_line_number, Some(2));
    assert_eq!(first_hunk.lines[1].new_line_number, Some(2));
    assert_eq!(first_hunk.lines[2].kind, ProjectGitDiffLineKind::Added);
    assert_eq!(first_hunk.lines[2].text, "line2 inserted");
    assert_eq!(first_hunk.lines[2].old_line_number, None);
    assert_eq!(first_hunk.lines[2].new_line_number, Some(3));
    assert_eq!(first_hunk.lines[5].text, "line5");
    assert_eq!(first_hunk.lines[5].old_line_number, Some(5));
    assert_eq!(first_hunk.lines[5].new_line_number, Some(6));

    let second_hunk = &file.hunks[1];
    assert_eq!(second_hunk.old_start, 9);
    assert_eq!(second_hunk.old_count, 4);
    assert_eq!(second_hunk.new_start, 10);
    assert_eq!(second_hunk.new_count, 5);
    assert_eq!(second_hunk.lines.len(), 5);
    assert_eq!(second_hunk.lines[0].text, "line9");
    assert_eq!(second_hunk.lines[0].old_line_number, Some(9));
    assert_eq!(second_hunk.lines[0].new_line_number, Some(10));
    assert_eq!(second_hunk.lines[3].kind, ProjectGitDiffLineKind::Added);
    assert_eq!(second_hunk.lines[3].text, "line11 inserted");
    assert_eq!(second_hunk.lines[3].old_line_number, None);
    assert_eq!(second_hunk.lines[3].new_line_number, Some(13));
    assert_eq!(second_hunk.lines[4].text, "line12");
    assert_eq!(second_hunk.lines[4].old_line_number, Some(12));
    assert_eq!(second_hunk.lines[4].new_line_number, Some(14));

    for hunk in &file.hunks {
        let old_numbers: Vec<u32> = hunk
            .lines
            .iter()
            .filter_map(|line| line.old_line_number)
            .collect();
        let new_numbers: Vec<u32> = hunk
            .lines
            .iter()
            .filter_map(|line| line.new_line_number)
            .collect();
        assert!(old_numbers.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(new_numbers.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[tokio::test]
async fn project_read_diff_full_file_mode_returns_single_hunk_spanning_file() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-diff-full-file-mode",
        &[(
            "src/main.rs",
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\n",
        )],
    );
    let updated_contents = "line1\nline2\nline2 inserted\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline11 inserted\nline12\n";
    write_file(&repo.path().join("src/main.rs"), updated_contents);

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff Full File Mode",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::FullFile,
        },
        "full file mode diff",
    )
    .await;

    assert_eq!(diff.context_mode, DiffContextMode::FullFile);
    assert_eq!(diff.files.len(), 1);
    let file = &diff.files[0];
    assert_eq!(file.hunks.len(), 1);
    let hunk = &file.hunks[0];
    assert_eq!(hunk.old_start, 1);
    assert_eq!(hunk.old_count, 12);
    assert_eq!(hunk.new_start, 1);
    assert_eq!(hunk.new_count, 14);
    assert_eq!(hunk.lines.len(), 14);
}

#[tokio::test]
async fn project_read_diff_payload_echoes_context_mode() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-diff-echo-mode",
        &[(
            "src/main.rs",
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\n",
        )],
    );
    write_file(
        &repo.path().join("src/main.rs"),
        "line1\nline2\nline2 inserted\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline11 inserted\nline12\n",
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Read Diff Echo Mode",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let hunks_diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "echo hunks mode diff",
    )
    .await;
    assert_eq!(hunks_diff.context_mode, DiffContextMode::Hunks);
    assert_eq!(hunks_diff.files.len(), 1);
    assert_eq!(hunks_diff.files[0].relative_path, "src/main.rs");
    assert_eq!(hunks_diff.files[0].hunks.len(), 2);
    assert_eq!(hunks_diff.files[0].hunks[0].old_start, 1);
    assert_eq!(hunks_diff.files[0].hunks[0].old_count, 5);
    assert_eq!(hunks_diff.files[0].hunks[0].new_start, 1);
    assert_eq!(hunks_diff.files[0].hunks[0].new_count, 6);
    assert_eq!(hunks_diff.files[0].hunks[0].lines[2].text, "line2 inserted");
    assert_eq!(
        hunks_diff.files[0].hunks[0].lines[2].new_line_number,
        Some(3)
    );
    assert_eq!(hunks_diff.files[0].hunks[1].old_start, 9);
    assert_eq!(hunks_diff.files[0].hunks[1].old_count, 4);
    assert_eq!(hunks_diff.files[0].hunks[1].new_start, 10);
    assert_eq!(hunks_diff.files[0].hunks[1].new_count, 5);
    assert_eq!(
        hunks_diff.files[0].hunks[1].lines[3].text,
        "line11 inserted"
    );
    assert_eq!(
        hunks_diff.files[0].hunks[1].lines[3].new_line_number,
        Some(13)
    );

    let full_file_diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::FullFile,
        },
        "echo full file mode diff",
    )
    .await;
    assert_eq!(full_file_diff.context_mode, DiffContextMode::FullFile);
    assert_eq!(full_file_diff.files.len(), 1);
    assert_eq!(full_file_diff.files[0].relative_path, "src/main.rs");
    assert_eq!(full_file_diff.files[0].hunks.len(), 1);
    let full_file_hunk = &full_file_diff.files[0].hunks[0];
    assert_eq!(full_file_hunk.old_start, 1);
    assert_eq!(full_file_hunk.old_count, 12);
    assert_eq!(full_file_hunk.new_start, 1);
    assert_eq!(full_file_hunk.new_count, 14);
    assert_eq!(full_file_hunk.lines.len(), 14);
    assert_eq!(full_file_hunk.lines[0].text, "line1");
    assert_eq!(full_file_hunk.lines[0].old_line_number, Some(1));
    assert_eq!(full_file_hunk.lines[0].new_line_number, Some(1));
    assert_eq!(full_file_hunk.lines[2].text, "line2 inserted");
    assert_eq!(full_file_hunk.lines[2].old_line_number, None);
    assert_eq!(full_file_hunk.lines[2].new_line_number, Some(3));
    assert_eq!(full_file_hunk.lines[12].text, "line11 inserted");
    assert_eq!(full_file_hunk.lines[12].old_line_number, None);
    assert_eq!(full_file_hunk.lines[12].new_line_number, Some(13));
}

#[tokio::test]
async fn project_stage_file_updates_git_status_and_diffs() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "stage-file",
        &[("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")],
    );
    write_file(
        &repo.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Stage File",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    fixture
        .client
        .project_stage_file(
            &project.id,
            ProjectStageFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project.roots[0].clone()),
                    relative_path: "src/main.rs".to_owned(),
                },
            },
        )
        .await
        .expect("project_stage_file failed");

    let _ = expect_project_file_list(&mut fixture.client, "file list after stage file").await;
    let git_status =
        expect_project_git_status(&mut fixture.client, "git status after stage file").await;
    assert_eq!(git_status.roots.len(), 1);
    assert_eq!(git_status.roots[0].files.len(), 1);
    assert!(git_status.roots[0].files[0].staged.is_some());
    assert!(git_status.roots[0].files[0].unstaged.is_none());

    let staged = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Staged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "staged diff after stage file",
    )
    .await;
    assert_eq!(staged.scope, ProjectDiffScope::Staged);
    assert_eq!(staged.files.len(), 1);

    let unstaged = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "unstaged diff after stage file",
    )
    .await;
    assert_eq!(unstaged.scope, ProjectDiffScope::Unstaged);
    assert!(unstaged.files.is_empty());
}

#[tokio::test]
async fn project_stage_hunk_stages_only_one_hunk() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "stage-hunk",
        &[(
            "src/main.rs",
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\n",
        )],
    );
    write_file(
        &repo.path().join("src/main.rs"),
        "line1\nline2 changed\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11 changed\nline12\n",
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Stage Hunk",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Unstaged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "initial hunk diff",
    )
    .await;
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].hunks.len(), 2);
    let first_hunk = diff.files[0].hunks[0].hunk_id.clone();
    let staged_before = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            root: protocol::ProjectRootPath(project.roots[0].clone()),
            scope: ProjectDiffScope::Staged,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
        },
        "initial staged diff before stage hunk",
    )
    .await;
    assert!(staged_before.files.is_empty());

    fixture
        .client
        .project_stage_hunk(
            &project.id,
            ProjectStageHunkPayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project.roots[0].clone()),
                    relative_path: "src/main.rs".to_owned(),
                },
                hunk_id: first_hunk,
            },
        )
        .await
        .expect("project_stage_hunk failed");

    let _ = expect_project_file_list(&mut fixture.client, "file list after stage hunk").await;
    let git_status =
        expect_project_git_status(&mut fixture.client, "git status after stage hunk").await;
    assert_eq!(git_status.roots[0].files.len(), 1);
    assert!(git_status.roots[0].files[0].staged.is_some());
    assert!(git_status.roots[0].files[0].unstaged.is_some());

    let staged = expect_project_git_diff(&mut fixture.client, "staged diff after stage hunk").await;
    assert_eq!(staged.scope, ProjectDiffScope::Staged);
    assert_eq!(staged.files.len(), 1);
    assert_eq!(staged.files[0].hunks.len(), 1);

    let unstaged =
        expect_project_git_diff(&mut fixture.client, "unstaged diff after stage hunk").await;
    assert_eq!(unstaged.scope, ProjectDiffScope::Unstaged);
    assert_eq!(unstaged.files.len(), 1);
    assert_eq!(unstaged.files[0].hunks.len(), 1);
}

#[tokio::test]
async fn project_stream_emits_live_file_and_git_updates() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("live-updates", &[("src/main.rs", "fn main() {}\n")]);
    let _project = create_project_with_real_roots(
        &mut fixture.client,
        "Live Updates",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    write_file(&repo.path().join("src/new.rs"), "pub fn new_file() {}\n");

    let file_list = expect_project_file_list(&mut fixture.client, "live file list update").await;
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/new.rs")
    );

    let git_status = expect_project_git_status(&mut fixture.client, "live git status update").await;
    assert!(
        git_status.roots[0]
            .files
            .iter()
            .any(|file| file.relative_path == "src/new.rs" && file.untracked)
    );
}

#[tokio::test]
async fn project_list_dir_returns_deeper_entries() {
    let mut fixture = Fixture::new().await;
    // Create a repo with files 4 levels deep. The initial depth-2 listing will
    // show entries down to a/b/c/ but NOT a/b/c/hidden.rs (3 folders deep).
    let repo = init_git_repo(
        "list-dir",
        &[
            ("top.rs", "// top\n"),
            ("a/mid.rs", "// mid\n"),
            ("a/b/deep.rs", "// deep\n"),
            ("a/b/c/hidden.rs", "// hidden\n"),
        ],
    );

    let project = create_project(
        &mut fixture.client,
        "List Dir",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let baseline =
        expect_project_file_list(&mut fixture.client, "server-pushed baseline file list").await;
    let _ =
        expect_project_git_status(&mut fixture.client, "server-pushed baseline git status").await;
    let paths: Vec<&str> = baseline.roots[0]
        .entries
        .iter()
        .map(|e| e.relative_path.as_str())
        .collect();

    // Depth-2 listing: root contents + 2 levels of subdirectory contents
    assert!(paths.contains(&"top.rs"), "missing top.rs in {paths:?}");
    assert!(paths.contains(&"a"), "missing a in {paths:?}");
    assert!(paths.contains(&"a/mid.rs"), "missing a/mid.rs in {paths:?}");
    assert!(paths.contains(&"a/b"), "missing a/b in {paths:?}");
    assert!(
        paths.contains(&"a/b/deep.rs"),
        "missing a/b/deep.rs in {paths:?}"
    );
    assert!(paths.contains(&"a/b/c"), "missing a/b/c in {paths:?}");
    // 3 folders deep should NOT appear
    assert!(
        !paths.contains(&"a/b/c/hidden.rs"),
        "a/b/c/hidden.rs should NOT appear in depth-2 listing but got {paths:?}"
    );

    // Now request a listing of subdirectory "a/b/c" — this should return a/b/c/hidden.rs
    fixture
        .client
        .project_list_dir(
            &project.id,
            ProjectListDirPayload {
                root: ProjectRootPath(project.roots[0].clone()),
                path: "a/b/c".to_owned(),
            },
        )
        .await
        .expect("project_list_dir failed");

    let listing = expect_project_file_list(&mut fixture.client, "list_dir response").await;
    assert!(
        listing.roots[0]
            .entries
            .iter()
            .all(|e| e.op == FileEntryOp::Add),
        "all list_dir entries should be Add ops"
    );
    let deep_paths: Vec<&str> = listing.roots[0]
        .entries
        .iter()
        .map(|e| e.relative_path.as_str())
        .collect();
    assert!(
        deep_paths.contains(&"a/b/c/hidden.rs"),
        "a/b/c/hidden.rs should appear in list_dir response but got {deep_paths:?}"
    );
}

#[tokio::test]
async fn live_watcher_sends_full_snapshot_after_deleted_files() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "delete-poll",
        &[("keep.rs", "// keep\n"), ("remove_me.rs", "// remove\n")],
    );

    let _project = create_project_with_real_roots(
        &mut fixture.client,
        "Delete Poll",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    // Delete a file — the debounced watcher should send a fresh full snapshot without it.
    fs::remove_file(repo.path().join("remove_me.rs")).expect("failed to delete remove_me.rs");

    let file_list = expect_project_file_list(&mut fixture.client, "file list after deletion").await;
    let entries = &file_list.roots[0].entries;

    assert!(
        entries.iter().any(|e| e.relative_path == "keep.rs"),
        "keep.rs should remain in the full snapshot: {entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e.relative_path == "remove_me.rs"),
        "remove_me.rs should be absent from the full snapshot: {entries:?}"
    );
    assert!(
        entries.iter().all(|e| e.op == FileEntryOp::Add),
        "full snapshots contain only Add entries: {entries:?}"
    );
}

#[tokio::test]
async fn server_pushed_snapshots_send_all_add_ops() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("refresh-ops", &[("lib.rs", "// lib\n")]);

    let _project = create_project(
        &mut fixture.client,
        "Snapshot Ops",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let file_list = expect_project_file_list(&mut fixture.client, "server-pushed file list").await;
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .all(|e| e.op == FileEntryOp::Add),
        "server-pushed file list should contain only Add ops"
    );
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|e| e.relative_path == "lib.rs"),
        "lib.rs should be present in server-pushed listing"
    );
    let _ = expect_project_git_status(&mut fixture.client, "server-pushed git status").await;
}
