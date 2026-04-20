mod fixture;

use fixture::Fixture;
use protocol::{
    Envelope, FileEntryOp, FrameKind, Project, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectDiffScope, ProjectFileContentsPayload, ProjectFileListPayload,
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
/// Drains the proactive file list + git status events that the server
/// now pushes automatically after project creation.
async fn create_project_with_real_roots(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    let project = create_project(client, name, roots).await;
    // Server proactively pushes file list and git status for real roots
    let _ = expect_project_file_list(client, "proactive file list after create").await;
    let _ = expect_project_git_status(client, "proactive git status after create").await;
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
async fn invalid_project_create_closes_the_connection() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Invalid".to_owned(),
            roots: vec!["/tmp/dup".to_owned(), "/tmp/dup".to_owned()],
        })
        .await
        .expect("project_create write failed");

    loop {
        match fixture
            .client
            .next_event()
            .await
            .expect("next_event after invalid project_create failed")
        {
            None => break,
            Some(env)
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
            Some(env) => panic!(
                "invalid project_create should terminate the connection, got: {} on {}",
                env.kind, env.stream
            ),
        }
    }
}

#[tokio::test]
async fn late_joining_client_gets_proactive_file_list_for_real_projects() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("late-proactive", &[("src/lib.rs", "pub fn a() {}\n")]);

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Late Proactive",
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

    // And proactive file list + git status for real roots
    let file_list = expect_project_file_list(&mut late_client, "late proactive file list").await;
    assert_eq!(file_list.roots.len(), 1);
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );

    let git_status = expect_project_git_status(&mut late_client, "late proactive git status").await;
    assert_eq!(git_status.roots.len(), 1);
    assert!(git_status.roots[0].clean);
}

#[tokio::test]
async fn create_project_proactively_pushes_file_list_and_git_status() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("proactive", &[("src/lib.rs", "pub fn a() {}\n")]);

    let project = create_project(
        &mut fixture.client,
        "Proactive",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    // Server should proactively push file list and git status without a project_refresh
    let file_list =
        expect_project_file_list(&mut fixture.client, "proactive file list after create").await;
    assert_eq!(file_list.roots.len(), 1);
    assert_eq!(file_list.roots[0].root.0, project.roots[0]);
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );

    let git_status =
        expect_project_git_status(&mut fixture.client, "proactive git status after create").await;
    assert_eq!(git_status.roots.len(), 1);
    assert!(git_status.roots[0].clean);
}

#[tokio::test]
async fn project_refresh_emits_file_list_and_git_status_for_all_roots() {
    let mut fixture = Fixture::new().await;
    let repo_a = init_git_repo("repo-a", &[("src/lib.rs", "pub fn a() {}\n")]);
    let repo_b = init_git_repo("repo-b", &[("app/main.rs", "fn main() {}\n")]);

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Multi Root",
        vec![
            repo_a.path().to_string_lossy().to_string(),
            repo_b.path().to_string_lossy().to_string(),
        ],
    )
    .await;

    fixture
        .client
        .project_refresh(&project.id)
        .await
        .expect("project_refresh failed");

    let file_list = expect_project_file_list(&mut fixture.client, "project file list").await;
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

    expect_no_event(
        &mut fixture.client,
        Duration::from_millis(200),
        "invalid absolute project read should not emit a file payload",
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

    fixture
        .client
        .project_read_diff(
            &project.id,
            ProjectReadDiffPayload {
                root: protocol::ProjectRootPath(project.roots[0].clone()),
                scope: ProjectDiffScope::Unstaged,
                path: Some("src/main.rs".to_owned()),
            },
        )
        .await
        .expect("project_read_diff failed");

    let diff = expect_project_git_diff(&mut fixture.client, "project git diff").await;
    assert_eq!(diff.scope, ProjectDiffScope::Unstaged);
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

    let staged = expect_project_git_diff(&mut fixture.client, "staged diff after stage file").await;
    assert_eq!(staged.scope, ProjectDiffScope::Staged);
    assert_eq!(staged.files.len(), 1);

    let unstaged =
        expect_project_git_diff(&mut fixture.client, "unstaged diff after stage file").await;
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

    fixture
        .client
        .project_read_diff(
            &project.id,
            ProjectReadDiffPayload {
                root: protocol::ProjectRootPath(project.roots[0].clone()),
                scope: ProjectDiffScope::Unstaged,
                path: Some("src/main.rs".to_owned()),
            },
        )
        .await
        .expect("initial project_read_diff failed");
    let diff = expect_project_git_diff(&mut fixture.client, "initial hunk diff").await;
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].hunks.len(), 2);
    let first_hunk = diff.files[0].hunks[0].hunk_id.clone();

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

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "List Dir",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    // Re-request via ProjectRefresh to get a known baseline.
    fixture
        .client
        .project_refresh(&project.id)
        .await
        .expect("project_refresh failed");
    let baseline = expect_project_file_list(&mut fixture.client, "baseline file list").await;
    let _ = expect_project_git_status(&mut fixture.client, "baseline git status").await;
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
async fn live_poll_sends_remove_op_for_deleted_files() {
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

    // Delete a file — the next poll diff should contain a Remove op for it
    fs::remove_file(repo.path().join("remove_me.rs")).expect("failed to delete remove_me.rs");

    let file_list = expect_project_file_list(&mut fixture.client, "file list after deletion").await;
    let entries = &file_list.roots[0].entries;

    // The diff should contain exactly one Remove for remove_me.rs
    let removed: Vec<&str> = entries
        .iter()
        .filter(|e| e.op == FileEntryOp::Remove)
        .map(|e| e.relative_path.as_str())
        .collect();
    assert_eq!(
        removed,
        vec!["remove_me.rs"],
        "expected Remove op for remove_me.rs, got {removed:?}"
    );

    // No Add ops should be present (nothing was added)
    let added: Vec<&str> = entries
        .iter()
        .filter(|e| e.op == FileEntryOp::Add)
        .map(|e| e.relative_path.as_str())
        .collect();
    assert!(
        added.is_empty(),
        "no files were added, but got Add ops for {added:?}"
    );
}

#[tokio::test]
async fn full_refresh_sends_all_add_ops() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("refresh-ops", &[("lib.rs", "// lib\n")]);

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Refresh Ops",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    fixture
        .client
        .project_refresh(&project.id)
        .await
        .expect("project_refresh failed");

    let file_list = expect_project_file_list(&mut fixture.client, "refresh file list").await;
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .all(|e| e.op == FileEntryOp::Add),
        "project_refresh file list should contain only Add ops"
    );
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|e| e.relative_path == "lib.rs"),
        "lib.rs should be present in refresh listing"
    );
    let _ = expect_project_git_status(&mut fixture.client, "refresh git status").await;
}
