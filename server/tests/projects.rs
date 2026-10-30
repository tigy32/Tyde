mod fixture;

use fixture::{Fixture, next_frame_matching_on};
use protocol::{
    BrowseBootstrapListing, BrowseBootstrapPayload, CommandErrorCode, CommandErrorPayload,
    DiffContextMode, Envelope, FileEntryOp, FrameKind, GitBranchName, HostAbsPath,
    HostBrowseEntriesPayload, HostBrowseInitial, HostBrowseOpenedPayload, HostBrowseStartPayload,
    Project, ProjectAddRootPayload, ProjectBootstrapPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectDeleteRootPayload, ProjectDiffScope, ProjectFileContentsPayload,
    ProjectFileListPayload, ProjectFileView, ProjectFileViewPayload, ProjectFileViewState,
    ProjectFileViewStatePayload, ProjectFileWindow, ProjectGitDiffLineKind, ProjectGitDiffPayload,
    ProjectGitStatusPayload, ProjectId, ProjectNotifyPayload, ProjectOpenPathAction,
    ProjectOpenPathPayload, ProjectPath, ProjectReadDiffPayload, ProjectReadFilePayload,
    ProjectRenamePayload, ProjectReorderPayload, ProjectReorderScope, ProjectRootPath,
    ProjectRootStatus, ProjectStageFilePayload, ProjectStageHunkPayload, ReviewStatus,
    ReviewSummaryScope, Steering, SteeringId, SteeringNotifyPayload, SteeringScope,
    SteeringUpsertPayload, StreamPath, WorkbenchCreatePayload,
};
use server::store::project::ProjectStore;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn is_control_plane_noise(env: &Envelope) -> bool {
    matches!(
        env.kind,
        FrameKind::HostSettings
            | FrameKind::BackendCapacity
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList
            | FrameKind::WorkflowNotify
            | FrameKind::AgentsViewPreferencesNotify
            // The slow-Git flow has a live agent. Its host-level usage update
            // legitimately interleaves with the independent project's upsert.
            | FrameKind::TaskTokenUsage
            | FrameKind::ProjectEvent
    )
}

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    fixture::next_interesting_frame_on(client, context, is_control_plane_noise).await
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    fixture::assert_no_interesting_frame_on(client, duration, context, is_control_plane_noise)
        .await;
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    let env = expect_project_response(client, FrameKind::ProjectNotify, context).await;
    env.parse_payload()
        .expect("failed to parse ProjectNotifyPayload")
}

async fn expect_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    let env = expect_project_response(client, FrameKind::CommandError, context).await;
    env.parse_payload()
        .expect("failed to parse CommandErrorPayload")
}

async fn expect_project_bootstrap(
    client: &mut client::Connection,
    context: &str,
) -> ProjectBootstrapPayload {
    let env = expect_project_response(client, FrameKind::ProjectBootstrap, context).await;
    env.parse_payload()
        .expect("failed to parse ProjectBootstrapPayload")
}

async fn expect_project_file_list(
    client: &mut client::Connection,
    context: &str,
) -> ProjectFileListPayload {
    // An independent Git refresh emitted ProjectGitStatus before a staging
    // listing. Project streams allow these unsolicited state pushes (§4).
    let env = expect_project_response(client, FrameKind::ProjectFileList, context).await;
    env.parse_payload()
        .expect("failed to parse ProjectFileListPayload")
}

async fn expect_project_file_contents(
    client: &mut client::Connection,
    context: &str,
) -> ProjectFileContentsPayload {
    let env = expect_project_response(client, FrameKind::ProjectFileContents, context).await;
    env.parse_payload()
        .expect("failed to parse ProjectFileContentsPayload")
}

async fn expect_project_git_diff(
    client: &mut client::Connection,
    context: &str,
) -> ProjectGitDiffPayload {
    let env = expect_project_response(client, FrameKind::ProjectGitDiff, context).await;
    env.parse_payload()
        .expect("failed to parse ProjectGitDiffPayload")
}

async fn expect_project_command_error(
    client: &mut client::Connection,
    context: &str,
) -> protocol::CommandErrorPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::CommandError {
            return env
                .parse_payload()
                .expect("failed to parse CommandErrorPayload");
        }
    }
}

async fn expect_project_response(
    client: &mut client::Connection,
    expected: FrameKind,
    context: &str,
) -> Envelope {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == expected {
            return env;
        }
        if matches!(
            env.kind,
            FrameKind::ProjectBootstrap
                | FrameKind::ProjectFileList
                | FrameKind::ProjectGitStatus
                | FrameKind::CodeIntelOverview
        ) {
            continue;
        }
        assert_eq!(env.kind, expected);
    }
}

async fn expect_project_file_list_matching(
    client: &mut client::Connection,
    context: &str,
    mut matches: impl FnMut(&ProjectFileListPayload) -> bool,
) -> ProjectFileListPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::ProjectFileList {
            let payload = env
                .parse_payload()
                .expect("failed to parse ProjectFileListPayload");
            if matches(&payload) {
                return payload;
            }
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::ProjectGitStatus | FrameKind::CodeIntelOverview
        ) {
            continue;
        }
        assert_eq!(env.kind, FrameKind::ProjectFileList);
    }
}

async fn expect_project_git_status_matching(
    client: &mut client::Connection,
    context: &str,
    mut matches: impl FnMut(&ProjectGitStatusPayload) -> bool,
) -> ProjectGitStatusPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::ProjectGitStatus {
            let payload = env
                .parse_payload()
                .expect("failed to parse ProjectGitStatusPayload");
            if matches(&payload) {
                return payload;
            }
            continue;
        }
        if matches!(
            env.kind,
            FrameKind::ProjectFileList | FrameKind::CodeIntelOverview
        ) {
            continue;
        }
        assert_eq!(env.kind, FrameKind::ProjectGitStatus);
    }
}

async fn expect_browse_event(
    client: &mut client::Connection,
    browse_stream: &StreamPath,
    kind: FrameKind,
    context: &str,
) -> Envelope {
    next_frame_matching_on(client, context, |env| {
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env
                .parse_payload()
                .expect("failed to parse CommandErrorPayload while waiting for browse event");
            panic!(
                "command error while waiting for {context}: {} {}",
                error.operation, error.message
            );
        }
        env.stream == *browse_stream && env.kind == kind
    })
    .await
}

async fn open_browse_and_read_initial(
    client: &mut client::Connection,
    browse_stream: StreamPath,
    initial: HostBrowseInitial,
) -> (HostBrowseOpenedPayload, HostBrowseEntriesPayload) {
    client
        .host_browse_start(HostBrowseStartPayload {
            browse_stream: browse_stream.clone(),
            initial,
            include_hidden: false,
        })
        .await
        .expect("host_browse_start failed");

    let env = expect_browse_event(
        client,
        &browse_stream,
        FrameKind::BrowseBootstrap,
        "browse bootstrap",
    )
    .await;
    assert_eq!(env.seq, 0, "BrowseBootstrap must be seq 0");
    let bootstrap: BrowseBootstrapPayload = env
        .parse_payload()
        .expect("failed to parse BrowseBootstrapPayload");
    let entries = match bootstrap.listing {
        BrowseBootstrapListing::Entries { entries } => entries,
        BrowseBootstrapListing::Error { error } => panic!(
            "browse bootstrap carried an error listing: {:?} {}",
            error.code, error.message
        ),
    };
    (bootstrap.opened, entries)
}

fn browse_stream(name: &str) -> StreamPath {
    StreamPath(format!("/browse/{name}"))
}

fn host_home() -> String {
    std::env::var("HOME").expect("HOME must be set for host browse tests")
}

fn host_root() -> String {
    "/".to_owned()
}

async fn request_project_diff(
    client: &mut client::Connection,
    project_id: &protocol::ProjectId,
    payload: ProjectReadDiffPayload,
    context: &str,
) -> ProjectGitDiffPayload {
    let expected = payload.clone();
    client
        .project_read_diff(project_id, payload)
        .await
        .expect("project_read_diff failed");
    loop {
        let diff = expect_project_git_diff(client, context).await;
        // Project streams can also push remembered live diff refreshes; wait
        // for the response that echoes the command payload under test.
        if diff.root == expected.root
            && diff.request_id == expected.request_id
            && diff.scope == expected.scope
            && diff.revision == expected.revision
            && diff.path == expected.path
            && diff.context_mode == expected.context_mode
        {
            return diff;
        }
    }
}

async fn create_project(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots: roots.into_iter().map(ProjectRootPath).collect(),
        })
        .await
        .expect("project_create failed");

    match expect_project_notify(client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

fn project_roots(project: &Project) -> Vec<String> {
    project
        .root_paths()
        .into_iter()
        .map(|root| root.0)
        .collect()
}

fn project_root(project: &Project, index: usize) -> String {
    project.root_paths()[index].0.clone()
}

/// Create a project with real filesystem roots.
/// Drains the server-pushed project bootstrap.
async fn create_project_with_real_roots(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    let project = create_project(client, name, roots).await;
    let _ = expect_project_bootstrap(client, "initial server-pushed project bootstrap").await;
    drain_initial_project_state_pushes(client, "initial project state pushes").await;
    project
}

async fn drain_initial_project_state_pushes(client: &mut client::Connection, context: &str) {
    loop {
        match tokio::time::timeout(Duration::from_millis(100), client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => panic!("next_event failed while draining {context}: {err:?}"),
            Ok(Ok(Some(env)))
                if is_control_plane_noise(&env)
                    || matches!(
                        env.kind,
                        FrameKind::ProjectBootstrap
                            | FrameKind::ProjectFileList
                            | FrameKind::ProjectGitStatus
                            | FrameKind::CodeIntelOverview
                    ) =>
            {
                continue;
            }
            Ok(Ok(Some(env))) => panic!(
                "unexpected event while draining {context}: kind={} stream={}",
                env.kind, env.stream
            ),
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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(root: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout)
        .expect("git output was not UTF-8")
        .trim()
        .to_owned()
}

fn git_with_stdin(root: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("git {:?} failed to spawn: {err}", args));
    let mut stdin_pipe = child.stdin.take().expect("git stdin");
    stdin_pipe.write_all(stdin).expect("write git stdin");
    drop(stdin_pipe);
    let output = child.wait_with_output().expect("wait for git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_expect_failure(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {:?}: {}", args, err));
    assert!(
        !output.status.success(),
        "git {:?} unexpectedly succeeded",
        args
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
    let primary = init_git_repo("startup-primary", &[("README.md", "primary")]);
    let obsolete = init_git_repo("startup-obsolete", &[("obsolete.txt", "obsolete")]);
    let (reached, arrived) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let _hook = fixture.on_project_scan(
        primary.path().to_path_buf(),
        server::ScanPoint::ActorStarting,
        move || {
            reached.send(()).unwrap();
            held.recv().unwrap();
        },
    );
    let project = create_project(
        &mut fixture.client,
        "Tyde",
        vec![
            primary.path().to_string_lossy().into_owned(),
            obsolete.path().to_string_lossy().into_owned(),
        ],
    )
    .await;
    assert!(!project.id.0.is_empty());
    assert_eq!(project.name, "Tyde");
    arrived.await.unwrap();
    fixture
        .client
        .project_delete_root(ProjectDeleteRootPayload {
            id: project.id.clone(),
            root: project.root_paths()[1].clone(),
        })
        .await
        .unwrap();
    match expect_project_notify(&mut fixture.client, "metadata adoption during held startup").await
    {
        ProjectNotifyPayload::Upsert { project: updated } => {
            assert_eq!(updated.root_paths(), vec![project.root_paths()[0].clone()])
        }
        _ => panic!("expected current full catalog record"),
    }
    release.send(()).unwrap();
    let bootstrap =
        expect_project_bootstrap(&mut fixture.client, "latest roots after startup adoption").await;
    assert_eq!(
        bootstrap.project.root_paths(),
        vec![project.root_paths()[0].clone()]
    );
    fs::write(primary.path().join("startup-settled.txt"), "settled").unwrap();
    fixture
        .client
        .project_stage_file(
            &project.id,
            ProjectStageFilePayload {
                path: ProjectPath {
                    root: project.root_paths()[0].clone(),
                    relative_path: "startup-settled.txt".into(),
                },
            },
        )
        .await
        .unwrap();
    let status = next_frame_matching_on(
        &mut fixture.client,
        "settled Git refresh after startup root adoption",
        |env| {
            env.kind == FrameKind::ProjectGitStatus
                && env
                    .parse_payload::<ProjectGitStatusPayload>()
                    .unwrap()
                    .roots
                    .iter()
                    .any(|root| {
                        root.files.iter().any(|file| {
                            file.relative_path == "startup-settled.txt" && file.staged.is_some()
                        })
                    })
        },
    )
    .await
    .parse_payload::<ProjectGitStatusPayload>()
    .unwrap();
    assert_eq!(
        status
            .roots
            .iter()
            .map(|root| root.root.clone())
            .collect::<Vec<_>>(),
        vec![project.root_paths()[0].clone()],
        "a startup-time record must not stamp obsolete roots with the new generation"
    );
    eprintln!(
        "PROJECT ROOT EVIDENCE: catalog changed during held actor startup; completed real Git refresh contains only current authorized roots"
    );
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
            roots: vec![ProjectRootPath("/tmp/shared".to_owned())],
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

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrap.projects;

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
            assert_eq!(project_roots(&renamed), vec!["/tmp/original".to_owned()]);
        }
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

fn rewrite_projects_json_with_foreign_record(path: &Path, foreign_id: &str) {
    let contents = std::fs::read_to_string(path).expect("read projects.json");
    let value: serde_json::Value = serde_json::from_str(&contents).expect("parse projects.json");
    let write_seq = value
        .get("write_seq")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(2);
    let Some(serde_json::Value::Object(mut records)) = value.get("records").cloned() else {
        panic!("projects.json records must be an object");
    };
    records.insert(
        foreign_id.to_owned(),
        serde_json::json!({
            "id": foreign_id,
            "name": "Foreign",
            "sort_order": 99,
            "source": {
                "kind": "standalone",
                "roots": ["/tmp/foreign-project"]
            }
        }),
    );
    #[derive(serde::Serialize)]
    struct ProjectsFile<'a> {
        version: u64,
        records: &'a serde_json::Map<String, serde_json::Value>,
        write_seq: u64,
    }
    let body = serde_json::to_string_pretty(&ProjectsFile {
        write_seq: write_seq + 1,
        version,
        records: &records,
    })
    .expect("serialize foreign projects.json");
    std::fs::write(path, body).expect("write projects.json");
}

#[tokio::test]
async fn project_store_keeps_foreign_records_across_rename() {
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        "Original",
        vec!["/tmp/original".to_owned()],
    )
    .await;

    let projects_path = fixture.store_dir().join("projects.json");
    rewrite_projects_json_with_foreign_record(&projects_path, "foreign-project");

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
        }
        other => panic!("expected upsert project notification, got {other:?}"),
    }

    let store = ProjectStore::load(projects_path).expect("load project store");
    let projects = store.list().expect("list projects");
    assert_eq!(projects.len(), 2, "rename must keep the foreign project");
    let renamed = projects
        .iter()
        .find(|candidate| candidate.id == project.id)
        .expect("renamed project");
    assert_eq!(renamed.name, "Renamed");
    let foreign = projects
        .iter()
        .find(|candidate| candidate.id.0 == "foreign-project")
        .expect("foreign project");
    assert_eq!(foreign.name, "Foreign");
}

#[tokio::test]
async fn slow_client_chat_backlog_is_bounded_by_bytes_not_frames() {
    for (name_bytes, updates, should_close) in [(32, 512, false), (64 * 1024, 140, true)] {
        let mut fixture = Fixture::new().await;
        let project = create_project(
            &mut fixture.client,
            &"p".repeat(name_bytes),
            vec!["/tmp/chat-backlog".to_owned()],
        )
        .await;
        let mut slow = fixture.connect().await;

        // The fixture transport holds only 8 KiB. Not reading this client
        // forces the remaining notifications into the server's chat queue.
        // Reorders fan out chat notifications without a control-lane burst.
        for _ in 0..updates {
            fixture
                .client
                .project_reorder(ProjectReorderPayload {
                    scope: ProjectReorderScope::TopLevel,
                    project_ids: vec![project.id.clone()],
                })
                .await
                .expect("send backlog-producing reorder");
            let notification =
                expect_project_notify(&mut fixture.client, "healthy client reorder").await;
            assert!(
                matches!(notification, ProjectNotifyPayload::Upsert { project: updated }
                    if updated.id == project.id && updated.name == project.name),
                "healthy client must receive every update unchanged"
            );
        }

        let mut received = 0;
        (async {
            loop {
                let Some(envelope) = slow.next_event().await.expect("read slow client") else {
                    assert!(
                        should_close,
                        "small chat updates must not disconnect the client"
                    );
                    break;
                };
                if envelope.kind != FrameKind::ProjectNotify {
                    continue;
                }
                let notification: ProjectNotifyPayload = envelope
                    .parse_payload()
                    .expect("parse queued project update");
                assert!(
                    matches!(notification, ProjectNotifyPayload::Upsert { project: updated }
                        if updated.id == project.id && updated.name == project.name),
                    "queued update must retain its complete payload"
                );
                received += 1;
                if !should_close && received == updates {
                    break;
                }
            }
        })
        .await;
        if should_close {
            assert!(
                received > 0 && received < updates,
                "byte overflow must stop delivery"
            );
        } else {
            assert_eq!(
                received, updates,
                "all small updates must survive the backlog"
            );
        }
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
            scope: ProjectReorderScope::TopLevel,
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

    let (_fresh_client, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let replayed = bootstrap.projects;

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
            root: ProjectRootPath("/tmp/root-b".to_owned()),
        })
        .await
        .expect("project_add_root failed");

    match expect_project_notify(&mut fixture.client, "project add root").await {
        ProjectNotifyPayload::Upsert { project: updated } => {
            assert_eq!(updated.id, project.id);
            assert_eq!(
                project_roots(&updated),
                vec!["/tmp/root-a".to_owned(), "/tmp/root-b".to_owned()]
            );
        }
        other => panic!("expected upsert project notification, got {other:?}"),
    }
    let initial = tempfile::tempdir().unwrap();
    let added = tempfile::tempdir().unwrap();
    fs::write(initial.path().join("original.txt"), "original root").unwrap();
    fs::write(added.path().join("added.txt"), "added root").unwrap();
    let project = create_project(
        &mut fixture.client,
        "Authoritative roots",
        vec![initial.path().to_string_lossy().into_owned()],
    )
    .await;
    next_frame_matching_on(
        &mut fixture.client,
        "root authority initial bootstrap",
        |env| {
            env.kind == FrameKind::ProjectBootstrap
                && env
                    .parse_payload::<ProjectBootstrapPayload>()
                    .is_ok_and(|bootstrap| bootstrap.project.id == project.id)
        },
    )
    .await;
    let mut observer = fixture.connect().await;
    next_frame_matching_on(
        &mut observer,
        "root authority observer subscription settled",
        |env| {
            env.kind == FrameKind::ProjectBootstrap
                && env
                    .parse_payload::<ProjectBootstrapPayload>()
                    .is_ok_and(|bootstrap| bootstrap.project.id == project.id)
        },
    )
    .await;
    let added_root = ProjectRootPath(added.path().to_string_lossy().into_owned());
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let scan_hook = fixture.on_project_scan(
        added.path().to_owned(),
        server::ScanPoint::ReadDirectory,
        move || {
            let _ = entered.send(());
            let _ = held.recv();
        },
    );
    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: project.id.clone(),
            root: added_root.clone(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut observer, "added root catalog publication", |env| {
        env.kind == FrameKind::ProjectNotify && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(), ProjectNotifyPayload::Upsert { project: updated } if updated.id == project.id && updated.root_paths().contains(&added_root))
    }).await;
    reached.await.unwrap();
    eprintln!("PROJECT ROOT EVIDENCE: real added-root scan held after catalog Upsert");
    observer
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: added_root.clone(),
                    relative_path: "added.txt".into(),
                },
            },
        )
        .await
        .unwrap();
    let read =
        expect_project_file_contents(&mut observer, "added-root authorization while scan held")
            .await;
    assert_eq!(read.contents.as_deref(), Some("added root"));
    eprintln!("PROJECT ROOT EVIDENCE: added-root read completed before scan release");
    fixture
        .client
        .project_delete_root(ProjectDeleteRootPayload {
            id: project.id.clone(),
            root: added_root.clone(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut observer, "removed root catalog publication", |env| {
        env.kind == FrameKind::ProjectNotify && matches!(env.parse_payload::<ProjectNotifyPayload>().unwrap(), ProjectNotifyPayload::Upsert { project: updated } if updated.id == project.id && !updated.root_paths().contains(&added_root))
    }).await;
    observer
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: added_root.clone(),
                    relative_path: "added.txt".into(),
                },
            },
        )
        .await
        .unwrap();
    let denied = expect_project_command_error(
        &mut observer,
        "removed-root authorization while obsolete scan held",
    )
    .await;
    assert_eq!(denied.operation, "project_read_file");
    assert!(denied.message.contains("does not belong"));
    observer
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: project.root_paths()[0].clone(),
                    relative_path: "original.txt".into(),
                },
            },
        )
        .await
        .unwrap();
    let read =
        expect_project_file_contents(&mut observer, "retained-root read while obsolete scan held")
            .await;
    assert_eq!(read.contents.as_deref(), Some("original root"));
    assert!(
        added.path().join("added.txt").is_file(),
        "root removal must not remove files"
    );
    eprintln!(
        "PROJECT ROOT EVIDENCE: removed-root rejection and retained-root read completed before obsolete scan release"
    );
    release.send(()).unwrap();
    drop(scan_hook);
    next_frame_matching_on(
        &mut observer,
        "latest root generation after obsolete scan completion",
        |env| {
            if env.kind != FrameKind::ProjectFileList
                || env.stream != StreamPath(format!("/project/{}", project.id))
            {
                return false;
            }
            let listing = env.parse_payload::<ProjectFileListPayload>().unwrap();
            assert!(
                listing.roots.iter().all(|root| root.root != added_root),
                "obsolete scan restored removed root membership"
            );
            listing.roots.len() == 1
                && listing.roots[0]
                    .entries
                    .iter()
                    .any(|entry| entry.relative_path == "original.txt")
        },
    )
    .await;
}

#[tokio::test]
async fn host_browse_home_opens_home() {
    let mut fixture = Fixture::new().await;
    let home = host_home();

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("home"),
        HostBrowseInitial::Home,
    )
    .await;

    assert_eq!(opened.home.0, home);
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, home);
}

#[tokio::test]
async fn host_browse_path_keeps_home_as_actual_home() {
    let mut fixture = Fixture::new().await;
    let initial = tempfile::tempdir().expect("create browse initial dir");
    let initial_path = initial.path().to_string_lossy().to_string();
    assert_ne!(
        initial_path,
        host_home(),
        "test initial path must differ from HOME"
    );

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("path"),
        HostBrowseInitial::Path {
            path: HostAbsPath(initial_path.clone()),
        },
    )
    .await;

    assert_eq!(opened.home.0, host_home());
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, initial_path);
}

#[tokio::test]
async fn host_browse_project_roots_with_one_root_opens_that_root() {
    let mut fixture = Fixture::new().await;
    let root = tempfile::tempdir().expect("create project root");
    let root_path = root.path().to_string_lossy().to_string();
    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Browse One Root",
        vec![root_path.clone()],
    )
    .await;

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("project-one-root"),
        HostBrowseInitial::ProjectRoots {
            project_id: project.id,
        },
    )
    .await;

    assert_eq!(opened.home.0, host_home());
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, root_path);
}

#[tokio::test]
async fn host_browse_project_roots_with_siblings_opens_common_parent() {
    let mut fixture = Fixture::new().await;
    let parent = tempfile::tempdir().expect("create project roots parent");
    let root_a = parent.path().join("root-a");
    let root_b = parent.path().join("root-b");
    fs::create_dir(&root_a).expect("create root-a");
    fs::create_dir(&root_b).expect("create root-b");
    let root_a_path = root_a.to_string_lossy().to_string();
    let root_b_path = root_b.to_string_lossy().to_string();
    let parent_path = parent.path().to_string_lossy().to_string();
    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Browse Sibling Roots",
        vec![root_a_path, root_b_path],
    )
    .await;

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("project-sibling-roots"),
        HostBrowseInitial::ProjectRoots {
            project_id: project.id,
        },
    )
    .await;

    assert_eq!(opened.home.0, host_home());
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, parent_path);
    assert!(
        entries.entries.iter().any(|entry| entry.name == "root-a"),
        "common parent listing should contain root-a"
    );
    assert!(
        entries.entries.iter().any(|entry| entry.name == "root-b"),
        "common parent listing should contain root-b"
    );
}

#[tokio::test]
async fn host_browse_project_roots_without_useful_parent_opens_home() {
    let mut fixture = Fixture::new().await;
    let home = host_home();
    let project = create_project(
        &mut fixture.client,
        "Browse Root Only Common Ancestor",
        vec![
            "/tyde-browse-no-common-a".to_owned(),
            "/tyde-browse-no-common-b".to_owned(),
        ],
    )
    .await;

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("project-no-useful-common-parent"),
        HostBrowseInitial::ProjectRoots {
            project_id: project.id,
        },
    )
    .await;

    assert_eq!(opened.home.0, home);
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, home);
}

#[tokio::test]
async fn host_browse_unknown_project_opens_home() {
    let mut fixture = Fixture::new().await;
    let home = host_home();

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("unknown-project"),
        HostBrowseInitial::ProjectRoots {
            project_id: ProjectId("does-not-exist".to_owned()),
        },
    )
    .await;

    assert_eq!(opened.home.0, home);
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, home);
}

#[tokio::test]
async fn host_browse_workbench_project_roots_opens_worktree() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("browse-workbench", &[("src/lib.rs", "pub fn a() {}\n")]);
    let parent = create_project_with_real_roots(
        &mut fixture.client,
        "Browse Workbench Parent",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    fixture
        .client
        .workbench_create(WorkbenchCreatePayload {
            parent_project_id: parent.id.clone(),
            branch: GitBranchName("feature/browse".to_owned()),
            name: "feature/browse".to_owned(),
        })
        .await
        .expect("workbench_create failed");
    let workbench = match expect_project_notify(&mut fixture.client, "workbench create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected workbench upsert, got {other:?}"),
    };
    let _ = expect_project_bootstrap(
        &mut fixture.client,
        "workbench server-pushed project bootstrap",
    )
    .await;
    let worktree_path = project_root(&workbench, 0);
    assert_ne!(
        worktree_path,
        project_root(&parent, 0),
        "workbench worktree must differ from the parent root"
    );

    let (opened, entries) = open_browse_and_read_initial(
        &mut fixture.client,
        browse_stream("workbench-roots"),
        HostBrowseInitial::ProjectRoots {
            project_id: workbench.id,
        },
    )
    .await;

    assert_eq!(opened.home.0, host_home());
    assert_eq!(opened.root.0, host_root());
    assert_eq!(entries.path.0, worktree_path);
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

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.projects.is_empty(),
        "deleted project should not appear in HostBootstrap"
    );
    expect_no_event(
        &mut late_client,
        Duration::from_millis(150),
        "deleted project should not replay",
    )
    .await;
}

#[tokio::test]
async fn delete_project_removes_project_scoped_steering() {
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        "Delete Steering",
        vec!["/tmp/delete-steering".to_owned()],
    )
    .await;
    let steering = Steering {
        id: SteeringId("delete-steering".to_owned()),
        scope: SteeringScope::Project(project.id.clone()),
        title: "Project steering".to_owned(),
        content: "Keep project focused.".to_owned(),
    };

    fixture
        .client
        .steering_upsert(SteeringUpsertPayload {
            steering: steering.clone(),
        })
        .await
        .expect("steering_upsert failed");
    let env = expect_project_response(
        &mut fixture.client,
        FrameKind::SteeringNotify,
        "steering upsert",
    )
    .await;
    assert_eq!(env.kind, FrameKind::SteeringNotify);
    assert_eq!(
        env.parse_payload::<SteeringNotifyPayload>()
            .expect("parse SteeringNotify upsert"),
        SteeringNotifyPayload::Upsert {
            steering: steering.clone()
        }
    );

    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: project.id.clone(),
        })
        .await
        .expect("project_delete failed");
    let env = expect_project_response(
        &mut fixture.client,
        FrameKind::SteeringNotify,
        "steering delete",
    )
    .await;
    assert_eq!(env.kind, FrameKind::SteeringNotify);
    assert_eq!(
        env.parse_payload::<SteeringNotifyPayload>()
            .expect("parse SteeringNotify delete"),
        SteeringNotifyPayload::Delete {
            id: steering.id.clone()
        }
    );
    match expect_project_notify(&mut fixture.client, "project delete").await {
        ProjectNotifyPayload::Delete { project: deleted } => assert_eq!(deleted.id, project.id),
        other => panic!("expected delete project notification, got {other:?}"),
    }

    let (_late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.steering == fixture.bootstrap.steering,
        "project-scoped steering should not replay after project delete"
    );
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

    let (_fresh_client, bootstrap) = fixture.connect_fresh_host_with_bootstrap().await;
    let replayed = bootstrap.projects;

    assert_eq!(replayed, vec![project_a, project_b]);
}

#[tokio::test]
async fn invalid_project_create_surfaces_command_error_and_keeps_connection_alive() {
    let mut fixture = Fixture::new().await;

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Invalid".to_owned(),
            roots: vec![
                ProjectRootPath("/tmp/dup".to_owned()),
                ProjectRootPath("/tmp/dup".to_owned()),
            ],
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

    let (mut late_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap
            .projects
            .iter()
            .any(|replayed| replayed.id == project.id)
    );

    // Project state is server-pushed on subscription replay; the frontend does
    // not request a refresh.
    let ProjectBootstrapPayload {
        file_list,
        git_status,
        ..
    } = expect_project_bootstrap(&mut late_client, "late server-pushed project bootstrap").await;
    assert_eq!(file_list.roots.len(), 1);
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );

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
    let ProjectBootstrapPayload {
        file_list,
        git_status,
        review_summaries,
        ..
    } = expect_project_bootstrap(
        &mut fixture.client,
        "server-pushed project bootstrap after create",
    )
    .await;
    assert_eq!(file_list.roots.len(), 1);
    assert_eq!(file_list.roots[0].root.0, project_root(&project, 0));
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs")
    );

    assert_eq!(git_status.roots.len(), 1);
    assert!(git_status.roots[0].clean);

    assert_eq!(review_summaries.len(), 1);
    assert_eq!(review_summaries[0].scope, ReviewSummaryScope::Workspace);
    assert!(matches!(review_summaries[0].status, ReviewStatus::Draft));
}

#[cfg(unix)]
#[tokio::test]
async fn slow_git_refresh_keeps_host_connections_responsive() {
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        force_project_watch_limit: true,
        ..Default::default()
    })
    .await;
    let repo = init_git_repo("slow-git", &[("src/lib.rs", "pub fn a() {}\n")]);
    let project = create_project(
        &mut fixture.client,
        "Slow Git",
        vec![repo.path().to_string_lossy().into_owned()],
    )
    .await;
    expect_project_bootstrap(&mut fixture.client, "slow git bootstrap").await;
    expect_command_error(&mut fixture.client, "watch limit warning").await;

    let peer_repo = init_git_repo("slow-peer", &[("peer.rs", "// peer\n")]);
    create_project(
        &mut fixture.client,
        "Slow Peer",
        vec![peer_repo.path().to_string_lossy().into_owned()],
    )
    .await;
    expect_project_bootstrap(&mut fixture.client, "peer bootstrap").await;
    expect_command_error(&mut fixture.client, "peer watch limit warning").await;

    let agent = fixture
        .spawn_scripted(
            "pending-project-interrupt",
            server::backend::mock::MockScript::one(server::backend::mock::MockTurn::held_text(
                "waiting for interrupt",
            )),
        )
        .await;
    fixture.next_chat_event_matching(&agent, "held response", |event| {
        matches!(event, protocol::ChatEvent::StreamDelta(delta) if delta.text.contains("waiting for interrupt"))
    }).await;

    let monitor_directory = tempfile::Builder::new()
        .prefix("tyde-git-monitor-")
        .tempdir_in("/tmp")
        .unwrap();
    let (release, released) = tokio::sync::watch::channel(false);
    let stop = tokio_util::sync::CancellationToken::new();
    let mut monitors = tokio::task::JoinSet::new();
    let mut entries = Vec::new();
    for (index, repository) in [&repo, &peer_repo].into_iter().enumerate() {
        let address = monitor_directory.path().join(index.to_string());
        let listener = tokio::net::UnixListener::bind(&address).unwrap();
        let (entered, entry) = tokio::sync::oneshot::channel();
        entries.push(entry);
        let mut entered = Some(entered);
        let released = released.clone();
        let stopped = stop.clone();
        monitors.spawn(async move {
            let mut calls = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        if let Some(entered) = entered.take() {
                            let _ = entered.send(());
                        }
                        let mut released = released.clone();
                        calls.spawn(async move {
                            use tokio::io::AsyncWriteExt;
                            while !*released.borrow_and_update() {
                                released.changed().await.unwrap();
                            }
                            socket.write_all(b"R").await.unwrap();
                        });
                    }
                    completed = calls.join_next(), if !calls.is_empty() => {
                        completed.unwrap().unwrap();
                    }
                    _ = stopped.cancelled() => break,
                }
            }
            while let Some(completed) = calls.join_next().await {
                completed.unwrap();
            }
        });
        let hook = repository.path().join(".git/slow-fsmonitor");
        fs::write(
            &hook,
            format!(
                "#!/usr/bin/env python3\nimport pathlib, socket, sys\nif not pathlib.Path('.git/monitor-release').exists():\n with socket.socket(socket.AF_UNIX) as connection:\n  connection.connect({:?})\n  assert connection.recv(1) == b'R'\nsys.stdout.buffer.write(b'token\\0/\\0')\n",
                address.to_str().unwrap(),
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        git(
            repository.path(),
            &["config", "core.fsmonitor", hook.to_str().unwrap()],
        );
    }
    write_file(&repo.path().join("src/lib.rs"), "pub fn changed() {}\n");
    // Real fsmonitor calls remain blocked until the protocol assertions finish;
    // the old two-second watchdog released healthy Git under host load.
    for entry in entries {
        entry.await.expect("real Git entered fsmonitor");
    }
    let (mut connection, bootstrap) = fixture.connect_with_bootstrap().await;
    let late_agent_stream = bootstrap
        .agents
        .iter()
        .find(|candidate| candidate.agent_id == agent.new_agent.agent_id)
        .expect("late connection knows the running agent")
        .instance_stream
        .clone();
    assert!(
        bootstrap
            .projects
            .iter()
            .any(|candidate| candidate.id == project.id)
    );
    assert!(
        !repo.path().join(".git/monitor-release").exists(),
        "host connection waited for the slow Git refresh to finish"
    );
    let path = ProjectPath {
        root: ProjectRootPath(project_root(&project, 0)),
        relative_path: "src/lib.rs".to_owned(),
    };
    fixture
        .client
        .project_read_file(&project.id, ProjectReadFilePayload { path })
        .await
        .unwrap();
    let contents = next_frame_matching_on(&mut fixture.client, "file during slow Git", |event| {
        event.kind == FrameKind::ProjectFileContents
    })
    .await;
    let contents: ProjectFileContentsPayload = contents.parse_payload().unwrap();
    assert_eq!(contents.contents.as_deref(), Some("pub fn changed() {}\n"));
    assert!(
        !repo.path().join(".git/monitor-release").exists(),
        "file read waited for the slow Git refresh to finish"
    );
    let independent_repo = init_git_repo("independent", &[("ready.rs", "// ready\n")]);
    create_project(
        &mut fixture.client,
        "Independent",
        vec![independent_repo.path().to_string_lossy().into_owned()],
    )
    .await;
    expect_project_bootstrap(
        &mut fixture.client,
        "independent project during stalled Git",
    )
    .await;
    expect_command_error(&mut fixture.client, "independent watch limit warning").await;
    assert!(
        !repo.path().join(".git/monitor-release").exists(),
        "independent project waited behind other projects' Git work"
    );
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: directory_view(&project_root(&project, 0), "src"),
                active: true,
            },
        )
        .await
        .unwrap();
    let ProjectFileViewState::Ready { entries, .. } = file_view_state(
        &mut fixture.client,
        &directory_view(&project_root(&project, 0), "src"),
    )
    .await
    else {
        panic!("folder failed");
    };
    assert!(
        entries
            .iter()
            .any(|entry| entry.path.relative_path == "src/lib.rs")
    );
    assert!(
        !repo.path().join(".git/monitor-release").exists(),
        "folder expansion waited for the slow Git refresh to finish"
    );
    connection
        .review_create(
            &project.id,
            protocol::ReviewCreatePayload {
                request_id: None,
                selection: protocol::ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
            },
        )
        .await
        .unwrap();
    next_frame_matching_on(
        &mut connection,
        "bootstrap before review response",
        |event| {
            assert_ne!(
                event.kind,
                FrameKind::ReviewBootstrap,
                "review response overtook project bootstrap"
            );
            event.kind == FrameKind::ProjectBootstrap
                && event.stream == StreamPath(format!("/project/{}", project.id))
        },
    )
    .await;
    assert!(
        !repo.path().join(".git/monitor-release").exists(),
        "project bootstrap waited for the slow Git refresh to finish"
    );
    connection.interrupt(&late_agent_stream).await.unwrap();
    fixture
        .next_chat_event_matching(&agent, "interrupt behind pending review", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
        })
        .await;
    assert!(
        !repo.path().join(".git/monitor-release").exists(),
        "interrupt waited behind a project subscription"
    );
    fs::write(repo.path().join(".git/monitor-release"), "").unwrap();
    fs::write(peer_repo.path().join(".git/monitor-release"), "").unwrap();
    release.send_replace(true);
    next_frame_matching_on(
        &mut connection,
        "review response after bootstrap",
        |event| event.kind == FrameKind::ReviewBootstrap,
    )
    .await;

    // The added agent also emits ContextCompactionCapability after interruption;
    // select the project's status without treating agent lifecycle frames as Git output.
    let status = next_frame_matching_on(&mut fixture.client, "status after slow Git", |event| {
        event.kind == FrameKind::ProjectGitStatus
            && event.stream == StreamPath(format!("/project/{}", project.id))
            && event
                .parse_payload::<ProjectGitStatusPayload>()
                .unwrap()
                .roots
                .iter()
                .any(|root| !root.clean)
    })
    .await;
    let status: ProjectGitStatusPayload = status.parse_payload().unwrap();
    assert_eq!(status.roots[0].files[0].relative_path, "src/lib.rs");
    drop(connection);
    stop.cancel();
    while let Some(completed) = monitors.join_next().await {
        completed.unwrap();
    }
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

    let ProjectBootstrapPayload {
        file_list,
        git_status,
        review_summaries,
        ..
    } = expect_project_bootstrap(&mut fixture.client, "server-pushed project bootstrap").await;
    assert_eq!(file_list.roots.len(), 2);
    assert_eq!(file_list.roots[0].root.0, project_root(&project, 0));
    assert_eq!(file_list.roots[1].root.0, project_root(&project, 1));
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

    assert_eq!(git_status.roots.len(), 2);
    assert!(git_status.roots.iter().all(|root| root.clean));
    assert!(git_status.roots.iter().all(|root| root.files.is_empty()));

    assert_eq!(review_summaries.len(), 1);
    assert_eq!(review_summaries[0].scope, ReviewSummaryScope::Workspace);
    assert!(matches!(review_summaries[0].status, ReviewStatus::Draft));
}

fn root_listing<'a>(
    file_list: &'a ProjectFileListPayload,
    root: &Path,
) -> &'a protocol::ProjectRootListing {
    file_list
        .roots
        .iter()
        .find(|listing| Path::new(&listing.root.0) == root)
        .unwrap_or_else(|| panic!("root {} missing from file list", root.display()))
}

fn root_unavailable_message(file_list: &ProjectFileListPayload, root: &Path) -> Option<String> {
    match &root_listing(file_list, root).status {
        ProjectRootStatus::Available => None,
        ProjectRootStatus::Unavailable { message } => Some(message.clone()),
    }
}

fn root_lists(file_list: &ProjectFileListPayload, root: &Path, relative_path: &str) -> bool {
    let listing = root_listing(file_list, root);
    listing.status == ProjectRootStatus::Available
        && listing
            .entries
            .iter()
            .any(|entry| entry.relative_path == relative_path)
}

/// One unreadable root must not take down the project: the healthy roots load
/// and stay live, the bad root is reported per root, and it can be removed.
#[tokio::test]
async fn unavailable_project_root_keeps_other_roots_live() {
    let mut fixture = Fixture::new().await;
    let repo_a = init_git_repo("healthy-a", &[("src/lib.rs", "pub fn a() {}\n")]);
    let repo_b = init_git_repo("healthy-b", &[("app/main.rs", "fn main() {}\n")]);
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("moved-away");

    let (adoption, adopted) =
        watch_completion(&fixture, repo_a.path(), server::ScanPoint::WatcherAdopted);
    let project = create_project(
        &mut fixture.client,
        "Missing Root",
        vec![
            repo_a.path().to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
            repo_b.path().to_string_lossy().into_owned(),
        ],
    )
    .await;
    let ProjectBootstrapPayload {
        file_list,
        git_status,
        ..
    } = expect_project_bootstrap(&mut fixture.client, "bootstrap with a missing root").await;
    assert_eq!(file_list.roots.len(), 3, "every configured root is listed");
    assert!(root_lists(&file_list, repo_a.path(), "src/lib.rs"));
    assert!(root_lists(&file_list, repo_b.path(), "app/main.rs"));
    let message = root_unavailable_message(&file_list, &missing)
        .expect("the missing root is reported unavailable");
    assert!(message.contains(missing.to_str().unwrap()));
    assert!(message.contains("No such file or directory"));
    assert!(root_listing(&file_list, &missing).entries.is_empty());
    let git_roots = git_status
        .roots
        .iter()
        .map(|root| root.root.0.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        git_roots,
        vec![project_root(&project, 0), project_root(&project, 2)],
        "Git status covers the readable roots only"
    );

    adopted.await.unwrap();
    drop(adoption);
    let healthy_path = ProjectPath {
        root: ProjectRootPath(project_root(&project, 0)),
        relative_path: "src/lib.rs".to_owned(),
    };
    let mut retired_reader = fixture.connect().await;
    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: ProjectRootPath(project_root(&project, 0)),
                    relative_path: "src/lib.rs".to_owned(),
                },
            },
        )
        .await
        .unwrap();
    let contents = expect_project_file_contents(&mut fixture.client, "healthy root read").await;
    assert_eq!(contents.contents.as_deref(), Some("pub fn a() {}\n"));

    write_file(&repo_a.path().join("src/added.rs"), "// added\n");
    expect_project_file_list_matching(&mut fixture.client, "healthy root stays watched", |list| {
        root_lists(list, repo_a.path(), "src/added.rs")
    })
    .await;

    let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked = fixture.on_project_scan(
        repo_b.path().join("app/retirement-trigger"),
        server::ScanPoint::WatcherReconcile,
        move || {
            let _ = blocked_tx.send(());
            release_rx.recv().expect("release retired watcher");
        },
    );
    fs::create_dir(repo_b.path().join("app/retirement-trigger")).unwrap();
    blocked_rx.await.unwrap();
    let (observation, mut waiting) = watch_completion(
        &fixture,
        &repo_a.path().join("src/lib.rs"),
        server::ScanPoint::FileObservationWaiting,
    );
    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: ProjectRootPath(project_root(&project, 0)),
                    relative_path: "src/lib.rs".to_owned(),
                },
            },
        )
        .await
        .unwrap();
    loop {
        tokio::select! {
            result = &mut waiting => { result.unwrap(); break; }
            event = fixture.client.next_event() => {
                let event = event.unwrap().unwrap();
                assert_ne!(event.kind, FrameKind::CommandError);
                assert_ne!(event.kind, FrameKind::ProjectFileContents, "read must queue observation on the adopted watcher");
            }
        }
    }
    drop(observation);
    let retired_path = ProjectPath {
        root: ProjectRootPath(project_root(&project, 2)),
        relative_path: "app/main.rs".to_owned(),
    };
    let (retired_observation, mut retired_waiting) = watch_completion(
        &fixture,
        &repo_b.path().join("app/main.rs"),
        server::ScanPoint::FileObservationWaiting,
    );
    retired_reader
        .project_read_file(&project.id, ProjectReadFilePayload { path: retired_path })
        .await
        .unwrap();
    loop {
        tokio::select! {
            result = &mut retired_waiting => { result.unwrap(); break; }
            event = retired_reader.next_event() => {
                let event = event.unwrap().unwrap();
                assert_ne!(event.kind, FrameKind::CommandError);
                assert_ne!(event.kind, FrameKind::ProjectFileContents, "unavailable-root read must queue before retirement");
            }
        }
    }
    drop(retired_observation);
    // The retained hang reached both observation waits, then filled the native
    // queue and scheduled a catch-up rescan. A per-path queued hook need not
    // run in that valid path. Fence native dispatch, retaining every protocol
    // assertion about replacement, queued reads and edit invalidation below.
    let (queued, changed) = watch_completion(
        &fixture,
        &repo_a.path().join("src/lib.rs"),
        server::ScanPoint::WatcherDispatch,
    );
    fs::write(repo_a.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    changed.await.unwrap();
    drop(queued);
    let drain_path = repo_b.path().join("app/dispatch-drain.rs");
    let (drain, drained) =
        watch_completion(&fixture, &drain_path, server::ScanPoint::WatcherDispatch);
    fs::write(&drain_path, "// native dispatch boundary\n").unwrap();
    drained.await.unwrap();
    drop(drain);
    let (replacement, adopted) =
        watch_completion(&fixture, repo_a.path(), server::ScanPoint::WatcherAdopted);
    fs::remove_dir_all(repo_b.path()).unwrap();
    adopted.await.unwrap();
    drop(replacement);
    let mut fresh_reader = fixture.connect().await;
    fresh_reader
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: healthy_path.clone(),
            },
        )
        .await
        .unwrap();
    let fresh =
        expect_project_file_contents(&mut fresh_reader, "read during held retirement").await;
    assert_eq!(fresh.contents.as_deref(), Some("pub fn changed() {}\n"));
    release_tx.send(()).unwrap();
    drop(blocked);
    let mut got_listing = false;
    let mut got_invalidation = false;
    let mut got_retired_read = false;
    while !got_listing || !got_invalidation || !got_retired_read {
        let event = fixture.client.next_event().await.unwrap().unwrap();
        assert_ne!(event.kind, FrameKind::CommandError);
        assert_ne!(event.kind, FrameKind::ProjectBootstrap);
        match event.kind {
            FrameKind::ProjectFileList => {
                let list: ProjectFileListPayload = event.parse_payload().unwrap();
                if root_unavailable_message(&list, repo_b.path()).is_some() {
                    assert!(root_lists(&list, repo_a.path(), "src/added.rs"));
                    got_listing = true;
                }
            }
            FrameKind::ProjectFileContents => {
                let retired: ProjectFileContentsPayload = event.parse_payload().unwrap();
                assert_eq!(retired.contents.as_deref(), Some("pub fn a() {}\n"));
                assert!(
                    fresh.version > retired.version,
                    "replacement must invalidate the queued healthy-root edit"
                );
                got_retired_read = true;
            }
            FrameKind::ProjectEvent => {
                if let protocol::ProjectEventPayload::FilesChanged { files } =
                    event.parse_payload().unwrap()
                {
                    got_invalidation |= files.iter().any(|file| {
                        file.path == healthy_path
                            && file.version > contents.version
                            && file.version <= fresh.version
                    });
                }
            }
            _ => {}
        }
    }
    let retired = expect_project_file_contents(
        &mut retired_reader,
        "unavailable-root observation completes",
    )
    .await;
    assert_eq!(retired.contents.as_deref(), Some("fn main() {}\n"));
    eprintln!(
        "PROJECT WATCH OWNER EVIDENCE actor_adopted=true retired_observation_completed=true queued_edit_invalidated=true"
    );
    expect_watched_write(
        &mut fixture.client,
        &project,
        "src/lib.rs",
        "pub fn still_watched() {}\n",
    )
    .await;
    write_file(&repo_a.path().join("src/after.rs"), "// after\n");
    expect_project_file_list_matching(
        &mut fixture.client,
        "healthy root survives another root's deletion",
        |list| root_lists(list, repo_a.path(), "src/after.rs"),
    )
    .await;

    fs::create_dir_all(missing.join("docs")).unwrap();
    fs::write(missing.join("docs/readme.md"), "back\n").unwrap();
    next_frame_matching_on(&mut fixture.client, "restored root is picked up", |env| {
        assert_ne!(env.kind, FrameKind::CommandError, "{env:?}");
        env.kind == FrameKind::ProjectFileList
            && root_lists(&env.parse_payload().unwrap(), &missing, "docs/readme.md")
    })
    .await;
    write_file(&missing.join("docs/later.md"), "watched\n");
    expect_project_file_list_matching(&mut fixture.client, "restored root is watched", |list| {
        root_lists(list, &missing, "docs/later.md")
    })
    .await;

    fixture
        .client
        .project_delete_root(ProjectDeleteRootPayload {
            id: project.id.clone(),
            root: ProjectRootPath(project_root(&project, 2)),
        })
        .await
        .unwrap();
    let file_list =
        next_frame_matching_on(&mut fixture.client, "unavailable root removed", |env| {
            assert_ne!(env.kind, FrameKind::CommandError, "{env:?}");
            env.kind == FrameKind::ProjectFileList
                && env
                    .parse_payload::<ProjectFileListPayload>()
                    .unwrap()
                    .roots
                    .len()
                    == 2
        })
        .await
        .parse_payload::<ProjectFileListPayload>()
        .unwrap();
    assert!(
        file_list
            .roots
            .iter()
            .all(|listing| listing.status == ProjectRootStatus::Available)
    );
}

#[tokio::test]
async fn project_read_file_returns_file_contents() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "read-file",
        &[("src/main.rs", "fn main() { println!(\"hi\"); }\n")],
    );
    std::fs::write(
        repo.path().join("preview.png"),
        b"\x89PNG\r\n\x1a\n\0test preview bytes",
    )
    .expect("write binary preview fixture");
    std::fs::write(repo.path().join("archive.bin"), b"\0unsupported bytes")
        .expect("write unsupported binary fixture");
    std::fs::write(repo.path().join("not-really.png"), b"ordinary text")
        .expect("write misleading extension fixture");
    let mut delayed_invalid = vec![b'a'; 9 * 1024 * 1024];
    delayed_invalid.push(0xff);
    std::fs::write(repo.path().join("delayed-invalid.dat"), delayed_invalid)
        .expect("write delayed invalid UTF-8 fixture");
    let mut m4a = b"\0\0\0\x18ftypM4A \0\0\0\0isomM4A ".to_vec();
    m4a.push(0xff);
    std::fs::write(repo.path().join("recording.data"), m4a).expect("write ISO BMFF audio fixture");
    let mut unknown_bmff = b"\0\0\0\x18ftypzzzz\0\0\0\0zzzzzzzz".to_vec();
    unknown_bmff.push(0xff);
    std::fs::write(repo.path().join("unknown-ftyp.data"), unknown_bmff)
        .expect("write unknown ISO BMFF fixture");
    std::fs::write(
        repo.path().join("oversized.bin"),
        vec![0_u8; 8 * 1024 * 1024 + 1],
    )
    .expect("write oversized binary fixture");
    let large_text = "large text line\n".repeat(600_000);
    assert!(large_text.len() > 8 * 1024 * 1024);
    std::fs::write(repo.path().join("large.txt"), &large_text).expect("write large text fixture");
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
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
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

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "preview.png".to_owned(),
                },
            },
        )
        .await
        .expect("binary project_read_file failed");
    let image = expect_project_file_contents(&mut fixture.client, "binary preview").await;
    let image = image.binary.expect("binary preview metadata");
    assert_eq!(image.mime_type, "image/png");
    assert_eq!(image.size_bytes, 27);
    assert!(image.data_base64.is_some(), "previewable bytes are carried");
    assert!(image.preview_error.is_none());

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "archive.bin".to_owned(),
                },
            },
        )
        .await
        .expect("unsupported project_read_file failed");
    let unsupported = expect_project_file_contents(&mut fixture.client, "unsupported binary").await;
    let unsupported = unsupported.binary.expect("unsupported binary metadata");
    assert_eq!(unsupported.mime_type, "application/octet-stream");
    assert!(unsupported.data_base64.is_none());
    assert!(unsupported.preview_error.is_some());

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "not-really.png".to_owned(),
                },
            },
        )
        .await
        .expect("misleading extension project_read_file failed");
    let misleading =
        expect_project_file_contents(&mut fixture.client, "misleading extension").await;
    assert!(
        !misleading.is_binary,
        "content beats the filename extension"
    );
    assert_eq!(misleading.contents.as_deref(), Some("ordinary text"));

    for (relative_path, expected_mime, has_preview) in [
        ("delayed-invalid.dat", "application/octet-stream", false),
        ("recording.data", "audio/mp4", true),
        ("unknown-ftyp.data", "application/octet-stream", false),
    ] {
        fixture
            .client
            .project_read_file(
                &project.id,
                ProjectReadFilePayload {
                    path: ProjectPath {
                        root: protocol::ProjectRootPath(project_root(&project, 0)),
                        relative_path: relative_path.to_owned(),
                    },
                },
            )
            .await
            .expect("classified binary project_read_file failed");
        let payload =
            expect_project_file_contents(&mut fixture.client, "classified binary preview").await;
        assert!(payload.is_binary, "{relative_path} must not render as text");
        let binary = payload.binary.expect("classified binary metadata");
        assert_eq!(binary.mime_type, expected_mime, "{relative_path}");
        assert_eq!(binary.data_base64.is_some(), has_preview, "{relative_path}");
    }

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "oversized.bin".to_owned(),
                },
            },
        )
        .await
        .expect("oversized project_read_file failed");
    let oversized = expect_project_file_contents(&mut fixture.client, "oversized binary").await;
    let oversized = oversized.binary.expect("oversized binary metadata");
    assert_eq!(oversized.size_bytes, 8 * 1024 * 1024 + 1);
    assert!(oversized.data_base64.is_none());
    assert!(
        oversized
            .preview_error
            .as_deref()
            .is_some_and(|message| message.contains("exceeds the 8 MiB limit"))
    );

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "large.txt".to_owned(),
                },
            },
        )
        .await
        .expect("large text project_read_file failed");
    let returned_large_text =
        expect_project_file_contents(&mut fixture.client, "large text contents").await;
    assert!(!returned_large_text.is_binary);
    assert_eq!(
        returned_large_text.contents.as_deref(),
        Some(large_text.as_str())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn project_open_path_dispatches_and_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let runtime_config = server::HostRuntimeConfig {
        project_path_opener_program: Some("/usr/bin/true".into()),
        ..server::HostRuntimeConfig::default()
    };
    let mut fixture = Fixture::new_with_runtime_config(runtime_config).await;
    let repo = init_git_repo("open-path", &[("文書/quote\" asset.bin", "inside")]);
    let outside = tempfile::tempdir().expect("outside tempdir");
    std::fs::write(outside.path().join("secret.bin"), b"\0outside binary")
        .expect("outside binary fixture");
    std::fs::write(outside.path().join("secret.txt"), b"outside text")
        .expect("outside text fixture");
    symlink(
        outside.path().join("secret.bin"),
        repo.path().join("escaped.bin"),
    )
    .expect("symlink fixture");
    symlink(
        outside.path().join("secret.txt"),
        repo.path().join("escaped.txt"),
    )
    .expect("text symlink fixture");
    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Open Path",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;
    let root = ProjectRootPath(project_root(&project, 0));

    for relative_path in ["escaped.bin", "escaped.txt"] {
        fixture
            .client
            .project_read_file(
                &project.id,
                ProjectReadFilePayload {
                    path: ProjectPath {
                        root: root.clone(),
                        relative_path: relative_path.to_owned(),
                    },
                },
            )
            .await
            .expect("escaped project_read_file send failed");
        let error = expect_command_error(&mut fixture.client, "read symlink escape error").await;
        assert_eq!(error.operation, "project_read_file");
        assert_eq!(error.code, CommandErrorCode::InvalidInput);
        assert!(error.message.contains("outside project root"));
    }

    for action in [
        ProjectOpenPathAction::OpenExternally,
        ProjectOpenPathAction::Reveal,
    ] {
        fixture
            .client
            .project_open_path(
                &project.id,
                ProjectOpenPathPayload {
                    path: ProjectPath {
                        root: root.clone(),
                        relative_path: "文書/quote\" asset.bin".to_owned(),
                    },
                    action,
                },
            )
            .await
            .expect("valid project_open_path failed");
    }

    fixture
        .client
        .project_open_path(
            &project.id,
            ProjectOpenPathPayload {
                path: ProjectPath {
                    root,
                    relative_path: "escaped.bin".to_owned(),
                },
                action: ProjectOpenPathAction::OpenExternally,
            },
        )
        .await
        .expect("escaped project_open_path send failed");
    let error = expect_command_error(&mut fixture.client, "symlink escape error").await;
    assert_eq!(error.operation, "project_open_path");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(error.message.contains("outside project root"));

    let linked_repo = init_git_repo(
        "linked-open-path-root",
        &[("文書/quote\" asset.bin", "linked")],
    );
    let linked_root_parent = tempfile::tempdir().expect("linked root parent");
    let linked_root = linked_root_parent.path().join("linked-project-root");
    symlink(linked_repo.path(), &linked_root).expect("linked project root");
    let linked_project = create_project_with_real_roots(
        &mut fixture.client,
        "Linked Root",
        vec![linked_root.to_string_lossy().to_string()],
    )
    .await;
    fixture
        .client
        .project_read_file(
            &linked_project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: ProjectRootPath(project_root(&linked_project, 0)),
                    relative_path: "文書/quote\" asset.bin".to_owned(),
                },
            },
        )
        .await
        .expect("symlinked-root project_read_file failed");
    let linked_contents =
        expect_project_file_contents(&mut fixture.client, "symlinked root contents").await;
    assert_eq!(linked_contents.contents.as_deref(), Some("linked"));
}

#[tokio::test]
async fn watch_limit_warns_without_stopping_project_or_polling_files() {
    let runtime_config = server::HostRuntimeConfig {
        force_project_watch_limit: true,
        ..server::HostRuntimeConfig::default()
    };
    let mut fixture = Fixture::new_with_runtime_config(runtime_config).await;
    let repo = init_git_repo(
        "watch-limit",
        &[("src/main.rs", "fn main() { println!(\"before\"); }\n")],
    );
    let project = create_project(
        &mut fixture.client,
        "Watch Limit",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let _ = expect_project_bootstrap(&mut fixture.client, "watch limit project bootstrap").await;
    let error = expect_command_error(&mut fixture.client, "watch limit warning").await;
    assert_eq!(error.operation, "project_watch");
    assert_eq!(error.code, CommandErrorCode::Internal);
    assert!(!error.fatal);
    assert!(
        error
            .message
            .contains("Live project file updates are disabled")
    );
    assert!(
        error
            .message
            .contains("configure each project root as the root of its Git repository")
    );
    assert!(error.message.contains("fs.inotify.max_user_watches"));
    assert!(!error.message.contains("fs.inotify.max_user_instances"));

    fs::write(
        repo.path().join("src/main.rs"),
        "fn main() { println!(\"after\"); }\n",
    )
    .expect("update file while project watching is unavailable");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(5_500);
    while let Ok(Ok(Some(env))) =
        tokio::time::timeout_at(deadline, fixture.client.next_event()).await
    {
        assert_ne!(
            env.kind,
            FrameKind::ProjectFileList,
            "degraded project must not poll the file tree"
        );
        if env.kind == FrameKind::ProjectEvent {
            let event: protocol::ProjectEventPayload = env
                .parse_payload()
                .expect("parse project event while degraded");
            assert!(
                !matches!(event, protocol::ProjectEventPayload::FilesChanged { .. }),
                "degraded project must not synthesize watched file changes"
            );
        }
    }

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "src/main.rs".to_owned(),
                },
            },
        )
        .await
        .expect("project_read_file should remain available");
    let contents = expect_project_file_contents(
        &mut fixture.client,
        "project file contents after watch limit",
    )
    .await;
    assert_eq!(
        contents.contents.as_deref(),
        Some("fn main() { println!(\"after\"); }\n")
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn project_recovers_after_real_file_descriptor_exhaustion() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("descriptor-recovery", &[("file.rs", "// before\n")]);
    // Process-wide exhaustion otherwise races the review-store creation needed
    // for bootstrap, testing setup failure rather than watcher recovery.
    let (begin_failure_tx, begin_failure_rx) = std::sync::mpsc::sync_channel(1);
    let initialization_fds = std::sync::Arc::new(std::sync::Mutex::new(None));
    let exhaust = initialization_fds.clone();
    let before_init = fixture.on_project_scan(
        repo.path().to_path_buf(),
        server::ScanPoint::WatcherInitialize,
        move || {
            begin_failure_rx
                .recv()
                .expect("bootstrap completes before watcher fault injection");
            *exhaust.lock().unwrap() = Some(fixture::ExhaustedFileDescriptors::exhaust());
        },
    );
    let release = initialization_fds.clone();
    let (initialized_tx, initialized_rx) = tokio::sync::oneshot::channel();
    let after_init = fixture.on_project_scan(
        repo.path().to_path_buf(),
        server::ScanPoint::WatcherInitialized,
        move || {
            drop(
                release
                    .lock()
                    .unwrap()
                    .take()
                    .expect("watcher creation exhausted descriptors"),
            );
            initialized_tx
                .send(())
                .expect("watcher initialization observer");
        },
    );
    let project = create_project(
        &mut fixture.client,
        "Descriptor Recovery",
        vec![repo.path().to_string_lossy().into_owned()],
    )
    .await;
    expect_project_bootstrap(&mut fixture.client, "descriptor bootstrap").await;
    begin_failure_tx
        .send(())
        .expect("release watcher fault injection");
    initialized_rx
        .await
        .expect("real watcher creation attempted");
    drop(before_init);
    drop(after_init);
    let event = next_frame_matching_on(
        &mut fixture.client,
        "watcher creation exhaustion guidance",
        |event| {
            event.kind == FrameKind::CommandError
                && event
                    .parse_payload::<CommandErrorPayload>()
                    .is_ok_and(|error| error.operation == "project_watch")
        },
    )
    .await;
    let error: CommandErrorPayload = event.parse_payload().unwrap();
    eprintln!(
        "watcher creation exhaustion: operation={} fatal={}",
        error.operation, error.fatal
    );
    assert!(!error.fatal);
    assert!(error.message.contains("Too many open files"));
    assert!(error.message.contains("fs.inotify.max_user_instances"));
    assert!(error.message.contains("shared"));
    assert!(error.message.contains("RLIMIT_NOFILE"));
    assert!(
        error
            .message
            .contains("sysctl fs.inotify.max_user_instances")
    );
    assert!(error.message.contains("administrator"));
    assert!(!error.message.contains("fs.inotify.max_user_watches"));
    assert!(error.message.contains("automatic recovery"));
    fs::write(repo.path().join("watch-ready"), "ready").unwrap();
    fixture::next_frame_matching_on(
        &mut fixture.client,
        "watcher initialized after exhaustion",
        |event| {
            if event.kind == FrameKind::CommandError {
                assert!(!event.parse_payload::<CommandErrorPayload>().unwrap().fatal);
            }
            assert_ne!(event.kind, FrameKind::ProjectBootstrap);
            event.kind == FrameKind::ProjectFileList
                && event
                    .parse_payload::<ProjectFileListPayload>()
                    .is_ok_and(|listing| {
                        listing.roots[0]
                            .entries
                            .iter()
                            .any(|entry| entry.relative_path == "watch-ready")
                    })
        },
    )
    .await;
    let held = std::sync::Arc::new(std::sync::Mutex::new(None));
    let exhaust = held.clone();
    let hook = fixture.on_project_scan(
        repo.path().to_path_buf(),
        server::ScanPoint::ReadDirectory,
        move || {
            *exhaust.lock().unwrap() = Some(fixture::ExhaustedFileDescriptors::exhaust());
        },
    );
    fs::write(
        repo.path().join("during-exhaustion"),
        "must appear after recovery",
    )
    .unwrap();
    let event = next_frame_matching_on(
        &mut fixture.client,
        "file scan descriptor exhaustion",
        |event| {
            if event.kind != FrameKind::CommandError {
                return false;
            }
            let error: CommandErrorPayload = event.parse_payload().unwrap();
            assert!(!error.fatal);
            error.operation == "project_files"
        },
    )
    .await;
    let error: CommandErrorPayload = event.parse_payload().unwrap();
    let exhausted = held
        .lock()
        .unwrap()
        .take()
        .expect("scan exhausted real descriptors");
    drop(exhausted);
    drop(hook);
    eprintln!(
        "descriptor recovery: operation={} fatal={}",
        error.operation, error.fatal
    );
    assert!(
        !error.fatal,
        "EMFILE must not terminate the project subscription"
    );
    assert!(error.message.contains("Too many open files"));
    assert!(error.message.contains("automatic recovery"));
    assert!(!error.message.contains("fs.inotify.max_user_instances"));
    // No new filesystem event or client command may be needed to retry the lost scan.
    (async {
        loop {
            let event = fixture.client.next_event().await.unwrap().unwrap();
            match event.kind {
                FrameKind::CommandError => {
                    let error: CommandErrorPayload = event.parse_payload().unwrap();
                    assert!(!error.fatal);
                }
                FrameKind::ProjectBootstrap => {
                    panic!("resource recovery must preserve the subscription")
                }
                FrameKind::ProjectFileList => {
                    let listing: ProjectFileListPayload = event.parse_payload().unwrap();
                    if listing.roots[0]
                        .entries
                        .iter()
                        .any(|entry| entry.relative_path == "during-exhaustion")
                    {
                        break;
                    }
                }
                _ => {}
            }
        }
    })
    .await;
    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: ProjectRootPath(project_root(&project, 0)),
                    relative_path: "file.rs".to_owned(),
                },
            },
        )
        .await
        .unwrap();
    let contents = expect_project_file_contents(&mut fixture.client, "file after EMFILE").await;
    assert_eq!(contents.contents.as_deref(), Some("// before\n"));
}

#[tokio::test]
async fn project_recovers_from_filesystem_and_git_failures_without_reconnecting() {
    fixture::init_tracing();
    for force_project_watch_limit in [false, true] {
        let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
            force_project_watch_limit,
            ..Default::default()
        })
        .await;
        let repo = init_git_repo("project-recovery", &[("file.rs", "// before\n")]);
        let project = create_project(
            &mut fixture.client,
            "Project Recovery",
            vec![repo.path().to_string_lossy().into_owned()],
        )
        .await;
        expect_project_bootstrap(&mut fixture.client, "recovery bootstrap").await;
        if force_project_watch_limit {
            expect_command_error(&mut fixture.client, "watch limit warning").await;
        }
        let path = ProjectPath {
            root: ProjectRootPath(project_root(&project, 0)),
            relative_path: "file.rs".to_owned(),
        };
        if !force_project_watch_limit {
            fs::write(repo.path().join("watch-ready"), "ready").unwrap();
            expect_project_file_list_matching(
                &mut fixture.client,
                "watcher initialized",
                |listing| {
                    listing.roots[0]
                        .entries
                        .iter()
                        .any(|entry| entry.relative_path == "watch-ready")
                },
            )
            .await;
        }
        // Initial watcher discovery can advance file versions. Establish the
        // unchanged-read baseline after that discovery, not before it.
        fixture
            .client
            .project_read_file(&project.id, ProjectReadFilePayload { path: path.clone() })
            .await
            .unwrap();
        let before = expect_project_file_contents(&mut fixture.client, "initial file").await;
        let healthy_repo = init_git_repo("healthy-during-recovery", &[("healthy.rs", "before\n")]);
        let healthy = if !force_project_watch_limit {
            Some(
                create_project_with_real_roots(
                    &mut fixture.client,
                    "Healthy during recovery",
                    vec![healthy_repo.path().to_string_lossy().into_owned()],
                )
                .await,
            )
        } else {
            None
        };
        let index = repo.path().join(".git/index");
        let saved_index = fs::read(&index).unwrap();
        fs::write(&index, "temporarily unreadable Git index").unwrap();
        let error = fixture::next_frame_matching_on(
            &mut fixture.client,
            "background I/O failure",
            |event| {
                event.kind == FrameKind::CommandError
                    && event.stream == StreamPath(format!("/project/{}", project.id))
                    && (force_project_watch_limit
                        || event
                            .parse_payload::<CommandErrorPayload>()
                            .is_ok_and(|error| error.operation == "project_watch"))
            },
        )
        .await;
        let error: CommandErrorPayload = error.parse_payload().unwrap();
        eprintln!(
            "project recovery: operation={} fatal={} watching={}",
            error.operation, error.fatal, !force_project_watch_limit
        );
        assert!(
            !error.fatal,
            "a failed background operation must not kill the project subscription"
        );
        assert!(matches!(
            error.operation.as_str(),
            "project_watch" | "project_git_status"
        ));
        assert!(!error.message.contains("fs.inotify.max_user_instances"));

        fixture
            .client
            .project_read_file(&project.id, ProjectReadFilePayload { path: path.clone() })
            .await
            .unwrap();
        let during = next_frame_matching_on(
            &mut fixture.client,
            "file during background failure",
            |event| {
                if event.kind == FrameKind::CommandError {
                    let error: CommandErrorPayload = event.parse_payload().unwrap();
                    assert!(!error.fatal);
                    assert_ne!(
                        error.operation, "project_read_file",
                        "background failure must not break file reads"
                    );
                }
                event.kind == FrameKind::ProjectFileContents
            },
        )
        .await;
        let during: ProjectFileContentsPayload = during.parse_payload().unwrap();
        assert_eq!(during.contents, before.contents);
        assert_eq!(during.version, before.version);

        if let Some(healthy) = &healthy {
            fs::write(
                healthy_repo.path().join("healthy.rs"),
                "healthy while another watcher fails\n",
            )
            .unwrap();
            next_frame_matching_on(&mut fixture.client, "healthy watcher survives another project's failure", |event| {
                if event.kind == FrameKind::CommandError {
                    assert_eq!(event.stream, StreamPath(format!("/project/{}", project.id)));
                    assert!(!event.parse_payload::<CommandErrorPayload>().unwrap().fatal);
                }
                event.stream == StreamPath(format!("/project/{}", healthy.id))
                    && event.kind == FrameKind::ProjectEvent
                    && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path.relative_path == "healthy.rs"))
            }).await;
            #[cfg(target_os = "linux")]
            expect_inotify_instances(2).await;
        }
        fs::write(repo.path().join("file.rs"), "// recovered\n").unwrap();
        fs::write(&index, saved_index).unwrap();
        let mut got_git = false;
        let mut got_change = force_project_watch_limit;
        (async {
            while !got_git || !got_change {
                let event = fixture.client.next_event().await.unwrap().unwrap();
                match event.kind {
                    FrameKind::CommandError => {
                        let error: CommandErrorPayload = event.parse_payload().unwrap();
                        assert!(
                            !error.fatal,
                            "recovery must retain the existing subscription"
                        );
                    }
                    FrameKind::ProjectBootstrap => {
                        panic!("recovery must not replace the subscription or reset file versions")
                    }
                    FrameKind::ProjectGitStatus => {
                        let status: ProjectGitStatusPayload = event.parse_payload().unwrap();
                        got_git |= event.stream == StreamPath(format!("/project/{}", project.id))
                            && status.roots.iter().any(|root| !root.clean);
                    }
                    FrameKind::ProjectEvent => {
                        if let protocol::ProjectEventPayload::FilesChanged { files } =
                            event.parse_payload().unwrap()
                        {
                            got_change |= files.iter().any(|change| {
                                change.path == path && change.version > before.version
                            });
                        }
                    }
                    _ => {}
                }
            }
        })
        .await;
        fixture
            .client
            .project_read_file(&project.id, ProjectReadFilePayload { path: path.clone() })
            .await
            .unwrap();
        let after = expect_project_file_contents(&mut fixture.client, "recovered file").await;
        assert_eq!(after.contents.as_deref(), Some("// recovered\n"));
        if !force_project_watch_limit {
            assert!(after.version > before.version);
            fs::write(repo.path().join("after-recovery.rs"), "// watching again\n").unwrap();
            expect_project_file_list_matching(
                &mut fixture.client,
                "watcher after recovery",
                |listing| {
                    listing.roots[0]
                        .entries
                        .iter()
                        .any(|entry| entry.relative_path == "after-recovery.rs")
                },
            )
            .await;
        }
    }
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
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: format!("{}/src/main.rs:366", project_root(&project, 0)),
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
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "/tmp/not-in-project.rs:12".to_owned(),
                },
            },
        )
        .await
        .expect("project_read_file failed");

    let error = expect_command_error(&mut fixture.client, "invalid absolute project read").await;
    assert!(
        matches!(&error.context, Some(protocol::CommandErrorContext::ProjectFile { path })
        if path.relative_path == "/tmp/not-in-project.rs:12" && path.root.0 == project_root(&project, 0))
    );
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
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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

    let status_before = git_stdout(repo.path(), &["status", "--porcelain"]);
    write_file(
        &repo.path().join("src/main.rs"),
        "fn main() {\n    println!(\"latest\");\n}\n",
    );
    assert_eq!(
        git_stdout(repo.path(), &["status", "--porcelain"]),
        status_before
    );
    let refreshed = (async {
        loop {
            let diff =
                expect_project_git_diff(&mut fixture.client, "live diff after second edit").await;
            if diff
                .files
                .iter()
                .flat_map(|file| &file.hunks)
                .flat_map(|hunk| &hunk.lines)
                .any(|line| {
                    line.kind == ProjectGitDiffLineKind::Added && line.text.contains("latest")
                })
            {
                return diff;
            }
        }
    })
    .await;
    assert_eq!(refreshed.path.as_deref(), Some("src/main.rs"));
    assert_eq!(refreshed.context_mode, DiffContextMode::Hunks);

    // "Expand context" reads the same diff at full context while the diff on
    // screen stays in hunk mode. That read is out of band: it answers itself
    // and leaves the refresh subscription alone. Recording its mode instead
    // would make every later refresh arrive in a mode the client discards as
    // stale, and the diff would silently stop updating.
    let expansion = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            request_id: Some("expand-context".to_owned()),
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::FullFile,
            out_of_band: true,
        },
        "out-of-band full-context diff",
    )
    .await;
    assert_eq!(expansion.context_mode, DiffContextMode::FullFile);
    assert!(
        expansion.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.text.contains("fn main()")),
        "the out-of-band read must still answer with the full-context diff"
    );

    write_file(
        &repo.path().join("src/main.rs"),
        "fn main() {\n    println!(\"newest\");\n}\n",
    );
    let after_expansion = (async {
        loop {
            let diff =
                expect_project_git_diff(&mut fixture.client, "live diff after expansion read")
                    .await;
            if diff.request_id.is_none()
                && diff
                    .files
                    .iter()
                    .flat_map(|file| &file.hunks)
                    .flat_map(|hunk| &hunk.lines)
                    .any(|line| {
                        line.kind == ProjectGitDiffLineKind::Added && line.text.contains("newest")
                    })
            {
                return diff;
            }
        }
    })
    .await;
    assert_eq!(
        after_expansion.context_mode,
        DiffContextMode::Hunks,
        "refreshes must keep arriving in the mode the displayed diff asked for"
    );
}

/// Full context turns a small hunk-mode diff into the whole file, and a big
/// enough file does not fit the one logical frame a response is sent in. The
/// encoder refuses an oversized frame by failing the writer task, and that
/// tears down the reader and the application with it — one click on "expand
/// context" would drop the host connection. The read has to refuse first, as
/// this request's error, with the connection still serving.
#[tokio::test]
async fn an_oversized_full_context_diff_fails_the_request_not_the_connection() {
    let mut fixture = Fixture::new().await;
    // Small on disk, but every line becomes a JSON object carrying its kind
    // and both line numbers, so at full context this serializes to well over
    // the 16 MiB a frame can hold.
    const LINES: usize = 320_000;
    let mut committed = String::with_capacity(LINES * 3);
    for _ in 0..LINES {
        committed.push_str("ab\n");
    }
    let repo = init_git_repo("oversized-diff", &[("src/big.txt", committed.as_str())]);
    write_file(
        &repo.path().join("src/big.txt"),
        &format!("changed\n{committed}"),
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Oversized Diff",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;
    let root = protocol::ProjectRootPath(project_root(&project, 0));
    let read = |context_mode, request_id: Option<&str>, out_of_band| ProjectReadDiffPayload {
        request_id: request_id.map(str::to_owned),
        root: root.clone(),
        scope: ProjectDiffScope::Unstaged,
        revision: protocol::ProjectDiffRevision::WorkingTree,
        path: Some("src/big.txt".to_owned()),
        context_mode,
        out_of_band,
    };

    // The diff the user is actually looking at is three lines of context and
    // fits comfortably.
    let hunks = request_project_diff(
        &mut fixture.client,
        &project.id,
        read(DiffContextMode::Hunks, None, false),
        "hunk-mode diff of a large file",
    )
    .await;
    assert_eq!(hunks.files.len(), 1);
    assert_eq!(hunks.files[0].relative_path, "src/big.txt");

    // Expanding context asks for the same diff at full context, which does
    // not fit. That must come back as this read's failure.
    fixture
        .client
        .project_read_diff(
            &project.id,
            read(DiffContextMode::FullFile, Some("expand-context"), true),
        )
        .await
        .expect("project_read_diff failed");
    let error =
        expect_project_command_error(&mut fixture.client, "oversized full-context read").await;
    assert_eq!(error.operation, "project_read_diff");
    assert!(
        error.message.contains("too large"),
        "the client has to be able to tell the user why: {}",
        error.message
    );
    assert!(
        !error.fatal,
        "an oversized read is this request's problem, not the connection's"
    );

    // And the connection is still there, still answering.
    let again = request_project_diff(
        &mut fixture.client,
        &project.id,
        read(DiffContextMode::Hunks, Some("after-refusal"), false),
        "hunk-mode diff after the refused expansion",
    )
    .await;
    assert_eq!(again.files.len(), 1);
    assert_eq!(again.files[0].relative_path, "src/big.txt");
}

#[tokio::test]
async fn project_read_diff_returns_typed_unmerged_file() {
    let mut fixture = Fixture::new().await;
    let name = "文書/quote\" conflict\tline\ndiff --git injected.rs";
    let binary = "文書/binary\" conflict.bin";
    let repo = init_git_repo(
        "read-diff-unmerged",
        &[(name, "base\n"), (binary, "\0base")],
    );
    git(repo.path(), &["config", "core.quotePath", "true"]);

    git(repo.path(), &["checkout", "-b", "other"]);
    write_file(&repo.path().join(name), "theirs\n");
    write_file(&repo.path().join(binary), "\0theirs");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "Change on other"]);
    git(repo.path(), &["checkout", "-"]);
    write_file(&repo.path().join(name), "ours\n");
    write_file(&repo.path().join(binary), "\0ours");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "Change on current"]);
    git_expect_failure(repo.path(), &["merge", "other"]);

    let project = create_project(
        &mut fixture.client,
        "Read Diff Unmerged",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut fixture.client, "conflict path bootstrap").await;
    assert_eq!(bootstrap.git_status.roots[0].files.len(), 2);
    for path in [name, binary] {
        let status = bootstrap.git_status.roots[0]
            .files
            .iter()
            .find(|file| file.relative_path == path)
            .unwrap();
        assert_eq!(
            status.staged,
            Some(protocol::ProjectGitChangeKind::Unmerged)
        );
        assert_eq!(
            status.unstaged,
            Some(protocol::ProjectGitChangeKind::Unmerged)
        );
    }
    let root = protocol::ProjectRootPath(project_root(&project, 0));
    let request = ProjectReadDiffPayload {
        request_id: None,
        root: root.clone(),
        scope: ProjectDiffScope::Unstaged,
        revision: protocol::ProjectDiffRevision::WorkingTree,
        path: Some(name.to_owned()),
        context_mode: DiffContextMode::Hunks,
        out_of_band: false,
    };

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        request.clone(),
        "typed unmerged project git diff",
    )
    .await;
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].relative_path, name);
    assert!(diff.files[0].unmerged);
    assert!(diff.files[0].hunks.is_empty());

    let binary_diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            path: Some(binary.to_owned()),
            out_of_band: true,
            ..request.clone()
        },
        "binary combined path",
    )
    .await;
    assert_eq!(binary_diff.files.len(), 1);
    assert_eq!(binary_diff.files[0].relative_path, binary);
    assert!(binary_diff.files[0].unmerged);
    assert!(binary_diff.files[0].is_binary);
    let staged = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            scope: ProjectDiffScope::Staged,
            out_of_band: true,
            ..request.clone()
        },
        "staged conflict marker with embedded newlines",
    )
    .await;
    assert!(staged.files.is_empty());

    write_file(&repo.path().join(name), "resolved\n");
    write_file(&repo.path().join(binary), "\0resolved");
    git(repo.path(), &["add", "."]);

    let resolved = request_project_diff(
        &mut fixture.client,
        &project.id,
        request,
        "resolved project git diff",
    )
    .await;
    assert!(
        resolved.files.iter().all(|file| !file.unmerged),
        "resolved and staged files must leave the typed unmerged state"
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/new.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: None,
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/new.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Staged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: None,
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::FullFile,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::FullFile,
            out_of_band: false,
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
    let names = [
        "Ångström/Ünïcödé/ŶÅÉgjpqy.rs",
        "文書/日本語ディレクトリ/設定ファイル.rs",
        "مجلد/عربي/changed.rs",
        "दस्तावेज़/เอกสาร/✨🚀/changed.rs",
        "white space/  quote\" tab\tline\n.txt",
        "文書/[a].txt",
        "文書/a.txt",
    ];
    for quote_paths in ["true", "false"] {
        let mut fixture = Fixture::new().await;
        let copy_source = "copy source\n".repeat(20);
        let mut initial = names.map(|name| (name, "before\n")).to_vec();
        initial.extend([
            ("旧 rename.txt", "rename source\n"),
            ("元の copy.txt", copy_source.as_str()),
            ("type 文書 \" name.txt", "regular file\n"),
            ("aaa b/zzz", "deleted contents\n"),
            ("aaa b/zzz b/aaa", "renamed contents\n"),
        ]);
        let repo = init_git_repo("path-actions", &initial);
        let base_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
        git(repo.path(), &["config", "core.quotePath", quote_paths]);
        for name in names {
            write_file(&repo.path().join(name), "after\n");
        }
        let project = create_project(
            &mut fixture.client,
            "Git path actions",
            vec![repo.path().to_str().unwrap().to_owned()],
        )
        .await;
        let bootstrap = expect_project_bootstrap(&mut fixture.client, "path bootstrap").await;
        let root = ProjectRootPath(project_root(&project, 0));
        let mut expected = names.to_vec();
        expected.sort();
        assert_eq!(
            bootstrap.git_status.roots[0]
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        eprintln!(
            "GIT_PATH_RECORDS quote_paths={quote_paths} bootstrap_paths={} exact=true",
            expected.len()
        );

        for (index, file) in bootstrap.git_status.roots[0].files.iter().enumerate() {
            let path = ProjectPath {
                root: root.clone(),
                relative_path: file.relative_path.clone(),
            };
            fixture
                .client
                .project_read_file(&project.id, ProjectReadFilePayload { path: path.clone() })
                .await
                .expect("open emitted Git path");
            let contents = expect_project_file_contents(&mut fixture.client, "open Git path").await;
            assert_eq!(contents.path, path);
            assert_eq!(contents.contents.as_deref(), Some("after\n"));
            let read = |scope| ProjectReadDiffPayload {
                request_id: None,
                root: root.clone(),
                scope,
                revision: protocol::ProjectDiffRevision::WorkingTree,
                path: Some(path.relative_path.clone()),
                context_mode: DiffContextMode::Hunks,
                out_of_band: true,
            };
            let diff = request_project_diff(
                &mut fixture.client,
                &project.id,
                read(ProjectDiffScope::Unstaged),
                "diff emitted Git path",
            )
            .await;
            assert_eq!(diff.files.len(), 1);
            assert_eq!(diff.files[0].relative_path, path.relative_path);
            assert_eq!(diff.files[0].hunks.len(), 1);
            if index == 0 {
                fixture
                    .client
                    .project_stage_hunk(
                        &project.id,
                        ProjectStageHunkPayload {
                            path: path.clone(),
                            hunk_id: diff.files[0].hunks[0].hunk_id.clone(),
                        },
                    )
                    .await
                    .expect("stage emitted hunk path");
            } else {
                fixture
                    .client
                    .project_stage_file(&project.id, ProjectStageFilePayload { path: path.clone() })
                    .await
                    .expect("stage emitted Git path");
            }
            expect_project_git_status_matching(
                &mut fixture.client,
                "staged exact Git path",
                |status| {
                    status.roots[0].files.iter().any(|file| {
                        file.relative_path == path.relative_path
                            && file.staged == Some(protocol::ProjectGitChangeKind::Modified)
                            && file.unstaged.is_none()
                    })
                },
            )
            .await;
            assert_eq!(
                git_stdout(repo.path(), &["show", &format!(":{}", path.relative_path)]),
                "after"
            );
            let staged = request_project_diff(
                &mut fixture.client,
                &project.id,
                read(ProjectDiffScope::Staged),
                "staged exact Git path",
            )
            .await;
            assert_eq!(staged.files.len(), 1);
            assert_eq!(staged.files[0].relative_path, path.relative_path);
            fixture::send_project_payload(
                &mut fixture.client,
                &project.id,
                FrameKind::ProjectUnstageFile,
                &protocol::ProjectUnstageFilePayload { path: path.clone() },
            )
            .await;
            expect_project_git_status_matching(
                &mut fixture.client,
                "unstaged exact Git path",
                |status| {
                    status.roots[0].files.iter().any(|file| {
                        file.relative_path == path.relative_path
                            && file.staged.is_none()
                            && file.unstaged == Some(protocol::ProjectGitChangeKind::Modified)
                    })
                },
            )
            .await;
            assert_eq!(
                git_stdout(repo.path(), &["show", &format!(":{}", path.relative_path)]),
                "before"
            );
            fixture::send_project_payload(
                &mut fixture.client,
                &project.id,
                FrameKind::ProjectDiscardFile,
                &protocol::ProjectDiscardFilePayload { path: path.clone() },
            )
            .await;
            expect_project_git_status_matching(
                &mut fixture.client,
                "discarded exact Git path",
                |status| {
                    !status.roots[0]
                        .files
                        .iter()
                        .any(|file| file.relative_path == path.relative_path)
                },
            )
            .await;
            assert_eq!(
                fs::read_to_string(repo.path().join(&path.relative_path)).unwrap(),
                "before\n"
            );
            for untouched in bootstrap.git_status.roots[0].files.iter().skip(index + 1) {
                assert_eq!(
                    fs::read_to_string(repo.path().join(&untouched.relative_path)).unwrap(),
                    "after\n"
                );
            }
        }
        assert!(git_stdout(repo.path(), &["status", "--porcelain"]).is_empty());
        eprintln!(
            "GIT_PATH_RECORDS quote_paths={quote_paths} open_diff_stage_unstage_discard=true"
        );
        #[cfg(unix)]
        {
            let type_path = repo.path().join("type 文書 \" name.txt");
            fs::remove_file(&type_path).unwrap();
            std::os::unix::fs::symlink(names[0], &type_path).unwrap();
        }
        git(repo.path(), &["config", "diff.renames", "copies"]);
        git(repo.path(), &["config", "status.renames", "copies"]);
        let renamed = "改名 \" destination\t.txt";
        let copied = "複製 \" destination.txt";
        let untracked = "未追跡 \" new\tline\n.txt";
        git(repo.path(), &["mv", "旧 rename.txt", renamed]);
        fs::remove_file(repo.path().join("aaa b/zzz")).unwrap();
        git(repo.path(), &["mv", "aaa b/zzz b/aaa", "zzz"]);
        write_file(&repo.path().join(copied), &copy_source);
        write_file(
            &repo.path().join("元の copy.txt"),
            &format!("{copy_source}changed\n"),
        );
        git(repo.path(), &["add", "."]);
        write_file(&repo.path().join(untracked), "untracked contents\n");
        let status = expect_project_git_status_matching(
            &mut fixture.client,
            "rename/copy records",
            |status| {
                status.roots[0].files.iter().any(|file| {
                    file.relative_path == renamed
                        && file.staged == Some(protocol::ProjectGitChangeKind::Renamed)
                }) && status.roots[0]
                    .files
                    .iter()
                    .any(|file| file.relative_path == untracked && file.untracked)
            },
        )
        .await;
        let mut expected = vec![
            renamed,
            copied,
            "元の copy.txt",
            untracked,
            "aaa b/zzz",
            "zzz",
        ];
        #[cfg(unix)]
        expected.push("type 文書 \" name.txt");
        expected.sort();
        assert_eq!(
            status.roots[0]
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            status.roots[0]
                .files
                .iter()
                .find(|file| file.relative_path == copied)
                .unwrap()
                .staged,
            Some(protocol::ProjectGitChangeKind::Copied)
        );
        assert_eq!(
            status.roots[0]
                .files
                .iter()
                .find(|file| file.relative_path == "元の copy.txt")
                .unwrap()
                .staged,
            Some(protocol::ProjectGitChangeKind::Modified)
        );
        let staged = request_project_diff(
            &mut fixture.client,
            &project.id,
            ProjectReadDiffPayload {
                request_id: None,
                root: root.clone(),
                scope: ProjectDiffScope::Staged,
                revision: protocol::ProjectDiffRevision::WorkingTree,
                path: None,
                context_mode: DiffContextMode::Hunks,
                out_of_band: true,
            },
            "rename/copy diff paths",
        )
        .await;
        assert_eq!(staged.files.len(), if cfg!(unix) { 7 } else { 5 });
        #[cfg(unix)]
        {
            assert_eq!(
                status.roots[0]
                    .files
                    .iter()
                    .find(|file| file.relative_path == "type 文書 \" name.txt")
                    .unwrap()
                    .staged,
                Some(protocol::ProjectGitChangeKind::TypeChanged)
            );
            assert_eq!(
                staged
                    .files
                    .iter()
                    .filter(|file| file.relative_path == "type 文書 \" name.txt")
                    .map(|file| file.change_kind)
                    .collect::<Vec<_>>(),
                vec![
                    Some(protocol::ProjectGitChangeKind::Deleted),
                    Some(protocol::ProjectGitChangeKind::Added)
                ]
            );
        }
        for (path, kind) in [
            (renamed, protocol::ProjectGitChangeKind::Renamed),
            ("aaa b/zzz", protocol::ProjectGitChangeKind::Deleted),
            ("zzz", protocol::ProjectGitChangeKind::Renamed),
            (copied, protocol::ProjectGitChangeKind::Copied),
            ("元の copy.txt", protocol::ProjectGitChangeKind::Modified),
        ] {
            assert_eq!(
                staged
                    .files
                    .iter()
                    .find(|file| file.relative_path == path)
                    .unwrap()
                    .change_kind,
                Some(kind)
            );
        }
        git(
            repo.path(),
            &["commit", "-m", "Rename and copy owned paths"],
        );
        let tip_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
        let history = request_project_diff(
            &mut fixture.client,
            &project.id,
            ProjectReadDiffPayload {
                request_id: None,
                root: root.clone(),
                scope: ProjectDiffScope::Uncommitted,
                revision: protocol::ProjectDiffRevision::CommittedRange { base_oid, tip_oid },
                path: None,
                context_mode: DiffContextMode::Hunks,
                out_of_band: true,
            },
            "History rename/copy diff paths",
        )
        .await;
        assert_eq!(history.files, staged.files);
        let untracked_diff = request_project_diff(
            &mut fixture.client,
            &project.id,
            ProjectReadDiffPayload {
                request_id: None,
                root: root.clone(),
                scope: ProjectDiffScope::Unstaged,
                revision: protocol::ProjectDiffRevision::WorkingTree,
                path: Some(untracked.to_owned()),
                context_mode: DiffContextMode::Hunks,
                out_of_band: true,
            },
            "untracked quoted path",
        )
        .await;
        assert_eq!(untracked_diff.files.len(), 1);
        assert_eq!(untracked_diff.files[0].relative_path, untracked);
        fixture::send_project_payload(
            &mut fixture.client,
            &project.id,
            FrameKind::ProjectDiscardFile,
            &protocol::ProjectDiscardFilePayload {
                path: ProjectPath {
                    root,
                    relative_path: untracked.to_owned(),
                },
            },
        )
        .await;
        expect_project_git_status_matching(
            &mut fixture.client,
            "discard untracked exact file",
            |status| status.roots[0].clean,
        )
        .await;
        assert!(!repo.path().join(untracked).exists());
        assert_eq!(
            fs::read_to_string(repo.path().join(copied)).unwrap(),
            copy_source
        );
        eprintln!("GIT_PATH_RECORDS quote_paths={quote_paths} rename_copy_history_untracked=true");
    }
}

async fn assert_project_stage_file_updates_git_status_and_diffs() {
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
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "src/main.rs".to_owned(),
                },
            },
        )
        .await
        .expect("project_stage_file failed");

    let file_list =
        expect_project_file_list(&mut fixture.client, "file list after stage file").await;
    assert_eq!(file_list.roots.len(), 1);
    assert_eq!(file_list.roots[0].root.0, project_root(&project, 0));
    let entry = file_list.roots[0]
        .entries
        .iter()
        .find(|entry| entry.relative_path == "src/main.rs")
        .expect("staged file remains listed");
    assert_eq!(entry.op, FileEntryOp::Add);
    let git_status = expect_project_git_status_matching(
        &mut fixture.client,
        "git status after stage file",
        |git_status| {
            git_status.roots[0].files.iter().any(|file| {
                file.relative_path == "src/main.rs"
                    && file.staged.is_some()
                    && file.unstaged.is_none()
            })
        },
    )
    .await;
    assert_eq!(git_status.roots.len(), 1);
    assert_eq!(git_status.roots[0].files.len(), 1);
    assert!(git_status.roots[0].files[0].staged.is_some());
    assert!(git_status.roots[0].files[0].unstaged.is_none());

    let staged = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Staged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
        },
        "unstaged diff after stage file",
    )
    .await;
    assert_eq!(unstaged.scope, ProjectDiffScope::Unstaged);
    assert!(unstaged.files.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn project_git_preserves_path_wrapper_without_forking_host() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FORKS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn count_fork() {
        FORKS.fetch_add(1, Ordering::Relaxed);
    }
    fn shell_quote(value: &std::ffi::OsStr) -> String {
        format!(
            "'{}'",
            value
                .to_str()
                .expect("test path UTF-8")
                .replace('\'', "'\\''")
        )
    }
    fn executable(path: &Path, body: &str) {
        // A preceding server flow can still be executing this inode.
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, body).expect("write wrapper");
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755))
            .expect("chmod wrapper");
        fs::rename(&replacement, path).expect("replace wrapper");
    }

    let original_path = std::env::var_os("PATH").expect("test PATH");
    let real_git = std::env::split_paths(&original_path)
        .map(|dir| dir.join("git"))
        .find(|path| {
            path.is_file()
                && rustix::fs::accessat(
                    rustix::fs::CWD,
                    path,
                    rustix::fs::Access::EXEC_OK,
                    rustix::fs::AtFlags::EACCESS,
                )
                .is_ok()
        })
        .expect("real Git installed");
    let real_git = std::path::absolute(real_git).expect("absolute real Git");
    let wrappers = tempfile::Builder::new()
        .prefix("git spawn wrappers ")
        .tempdir()
        .expect("wrapper directory");
    let denied = wrappers.path().join("not executable");
    let selected = wrappers.path().join("selected");
    fs::create_dir_all(&denied).expect("denied directory");
    fs::create_dir_all(&selected).expect("selected directory");
    fs::write(denied.join("git"), "not executable").expect("non-executable PATH entry");
    fs::set_permissions(denied.join("git"), fs::Permissions::from_mode(0o644))
        .expect("chmod non-executable entry");
    let calls = wrappers.path().join("calls");
    let wrapper = wrappers.path().join("host-git-wrapper");
    executable(
        &wrapper,
        &format!(
            "#!/bin/sh\n[ \"$LC_ALL\" = C ] || exit 91\nprintf '%s|%s\\n' \"$0\" \"$*\" >> {}\nexec {} \"$@\"\n",
            shell_quote(calls.as_os_str()),
            shell_quote(real_git.as_os_str()),
        ),
    );
    symlink(&wrapper, selected.join("git")).expect("symlink custom Git");
    let resolved_path = std::env::join_paths(
        [denied.clone(), selected.clone()]
            .into_iter()
            .chain(std::env::split_paths(&original_path)),
    )
    .expect("custom login PATH");
    let shell = wrappers.path().join("login-shell");
    executable(
        &shell,
        &format!(
            "#!/bin/sh\nprintf 'TYDE_SHELL_PROBE_BEGIN_7f3c9a2e=%s=TYDE_SHELL_PROBE_END_7f3c9a2e\\n' {}\n",
            shell_quote(&resolved_path),
        ),
    );
    // Nextest isolates each test in its own process. Set the login shell before
    // starting the runtime; the inherited PATH deliberately does not select our wrapper.
    unsafe {
        std::env::set_var("SHELL", &shell);
    }
    server::process_env::initialize_process_env().expect("resolve login PATH");
    // A prepare handler observes actual libc forks in this host process, not
    // forks that Git or the selected wrapper may perform after exec.
    assert_eq!(
        unsafe { libc::pthread_atfork(Some(count_fork), None, None) },
        0
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(assert_project_stage_file_updates_git_status_and_diffs());

    let calls = fs::read_to_string(calls).expect("custom Git was invoked");
    let lines = calls.lines().collect::<Vec<_>>();
    assert!(
        !lines.is_empty(),
        "Git must use the host's login PATH wrapper"
    );
    let selected_prefix = format!("{}|", selected.join("git").display());
    assert!(
        lines.iter().all(|line| line.starts_with(&selected_prefix)),
        "preserve the executable symlink, not its canonical target"
    );
    for command in ["status", "hash-object", "log", "diff", "add"] {
        assert!(
            lines.iter().any(|line| line
                .split_once('|')
                .expect("wrapper log entry")
                .1
                .split_whitespace()
                .any(|arg| arg == command)),
            "protocol flow must exercise {command}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|line| line.contains("|--no-optional-locks ")),
        "preserve read-only flags"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains(" add ") && !line.contains("--no-optional-locks")),
        "preserve mutating flags"
    );
    assert_eq!(
        FORKS.load(Ordering::Relaxed),
        0,
        "project Git operations must not run the host allocator's fork handlers"
    );

    #[cfg(target_env = "gnu")]
    {
        // glibc execvp accepts executable shell wrappers without a shebang.
        // The trusted helper's execvp preserves this shell-wrapper behavior
        // without copying the host address space or running its fork handlers.
        let wrapper_script = fs::read_to_string(&wrapper).expect("read wrapper");
        executable(
            &wrapper,
            wrapper_script
                .strip_prefix("#!/bin/sh\n")
                .expect("wrapper shebang"),
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fallback runtime")
            .block_on(assert_project_stage_file_updates_git_status_and_diffs());
        assert!(
            FORKS.load(Ordering::Relaxed) == 0,
            "legacy shell wrappers must not fork the host"
        );

        executable(&wrapper, &wrapper_script);
    }
    executable(
        &denied.join("git"),
        "#!/tyde-test-missing-interpreter\nexit 99\n",
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("PATH fallback runtime")
        .block_on(assert_project_stage_file_updates_git_status_and_diffs());
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Unstaged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
            request_id: None,
            root: protocol::ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Staged,
            revision: protocol::ProjectDiffRevision::WorkingTree,
            path: Some("src/main.rs".to_owned()),
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
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
                    root: protocol::ProjectRootPath(project_root(&project, 0)),
                    relative_path: "src/main.rs".to_owned(),
                },
                hunk_id: first_hunk,
            },
        )
        .await
        .expect("project_stage_hunk failed");

    // A subscribed ProjectGitDiff arrived before the next ProjectGitStatus.
    // Collect the independent pushes without discarding either diff or status.
    let (file_list, git_status, staged, unstaged) = (async {
        let mut file_list = None;
        let mut git_status = None;
        let mut staged = None;
        let mut unstaged = None;
        while file_list.is_none() || git_status.is_none() || staged.is_none() || unstaged.is_none()
        {
            let env = expect_next_event(&mut fixture.client, "stage hunk updates").await;
            match env.kind {
                FrameKind::ProjectFileList => {
                    file_list = Some(
                        env.parse_payload::<ProjectFileListPayload>()
                            .expect("staging file list"),
                    );
                }
                FrameKind::ProjectGitStatus => {
                    let status: ProjectGitStatusPayload =
                        env.parse_payload().expect("staging Git status");
                    if status.roots[0].files.iter().any(|file| {
                        file.relative_path == "src/main.rs"
                            && file.staged.is_some()
                            && file.unstaged.is_some()
                    }) {
                        git_status = Some(status);
                    }
                }
                FrameKind::ProjectGitDiff => {
                    let diff: ProjectGitDiffPayload = env.parse_payload().expect("staging diff");
                    assert_eq!(diff.root.0, project_root(&project, 0));
                    assert_eq!(diff.path.as_deref(), Some("src/main.rs"));
                    if diff.files.len() == 1 && diff.files[0].hunks.len() == 1 {
                        match diff.scope {
                            ProjectDiffScope::Staged => staged = Some(diff),
                            ProjectDiffScope::Unstaged => unstaged = Some(diff),
                            ProjectDiffScope::Uncommitted => {
                                panic!("unexpected combined diff for staging subscriptions")
                            }
                        }
                    }
                }
                FrameKind::CodeIntelOverview => {}
                other => panic!("unexpected stage hunk update: {other:?}"),
            }
        }
        (
            file_list.unwrap(),
            git_status.unwrap(),
            staged.unwrap(),
            unstaged.unwrap(),
        )
    })
    .await;
    assert_eq!(file_list.roots.len(), 1);
    assert_eq!(file_list.roots[0].root.0, project_root(&project, 0));
    let entry = file_list.roots[0]
        .entries
        .iter()
        .find(|entry| entry.relative_path == "src/main.rs")
        .expect("staged file remains listed");
    assert_eq!(entry.op, FileEntryOp::Add);
    assert_eq!(git_status.roots[0].files.len(), 1);
    assert!(git_status.roots[0].files[0].staged.is_some());
    assert!(git_status.roots[0].files[0].unstaged.is_some());

    assert_eq!(staged.scope, ProjectDiffScope::Staged);
    assert_eq!(staged.files.len(), 1);
    assert_eq!(staged.files[0].hunks.len(), 1);

    assert_eq!(unstaged.scope, ProjectDiffScope::Unstaged);
    assert_eq!(unstaged.files.len(), 1);
    assert_eq!(unstaged.files[0].hunks.len(), 1);
    assert!(
        staged.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.kind == ProjectGitDiffLineKind::Added
                && line.text.contains("line2 changed"))
    );
    assert!(
        unstaged.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.kind == ProjectGitDiffLineKind::Added
                && line.text.contains("line11 changed"))
    );
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

    let file_list = expect_project_file_list_matching(
        &mut fixture.client,
        "live file list update",
        |file_list| {
            file_list.roots[0]
                .entries
                .iter()
                .any(|entry| entry.relative_path == "src/new.rs")
        },
    )
    .await;
    assert!(
        file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/new.rs")
    );

    let git_status = expect_project_git_status_matching(
        &mut fixture.client,
        "live git status update",
        |git_status| {
            git_status.roots[0]
                .files
                .iter()
                .any(|file| file.relative_path == "src/new.rs" && file.untracked)
        },
    )
    .await;
    assert!(
        git_status.roots[0]
            .files
            .iter()
            .any(|file| file.relative_path == "src/new.rs" && file.untracked)
    );
}

fn directory_view(root: &str, path: &str) -> ProjectFileView {
    ProjectFileView::Directory {
        path: ProjectPath {
            root: ProjectRootPath(root.to_owned()),
            relative_path: path.to_owned(),
        },
        window: ProjectFileWindow::First,
        show_hidden: true,
        filter: String::new(),
    }
}

async fn file_view_state(
    client: &mut client::Connection,
    view: &ProjectFileView,
) -> ProjectFileViewState {
    file_view_state_matching(client, view, |_| true).await
}

async fn file_view_state_matching(
    client: &mut client::Connection,
    view: &ProjectFileView,
    matches: impl Fn(&ProjectFileViewState) -> bool,
) -> ProjectFileViewState {
    next_frame_matching_on(client, "settled file view", |event| {
        event.kind == FrameKind::ProjectFileViewState
            && event
                .parse_payload::<ProjectFileViewStatePayload>()
                .is_ok_and(|page| {
                    page.view == *view
                        && page.state != ProjectFileViewState::Loading
                        && matches(&page.state)
                })
    })
    .await
    .parse_payload::<ProjectFileViewStatePayload>()
    .unwrap()
    .state
}

async fn subscribe_file_view(
    client: &mut client::Connection,
    project: &ProjectId,
    view: ProjectFileView,
) -> ProjectFileViewState {
    client
        .project_file_view(
            project,
            ProjectFileViewPayload {
                view: view.clone(),
                active: true,
            },
        )
        .await
        .unwrap();
    file_view_state(client, &view).await
}

#[tokio::test]
async fn project_file_views_page_every_file_and_retain_visible_directories() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "file-views",
        &[("top.rs", "// top"), (".gitignore", "wide/\n")],
    );
    const FILES: usize = 10_050;
    fs::create_dir(repo.path().join("wide")).unwrap();
    for file in 0..FILES {
        fs::write(repo.path().join(format!("wide/f{file:05}")), "").unwrap();
    }
    for dir in 0..70 {
        fs::create_dir_all(repo.path().join(format!("deep/d{dir:02}/nested"))).unwrap();
        fs::write(
            repo.path()
                .join(format!("deep/d{dir:02}/nested/visible.rs")),
            "// visible",
        )
        .unwrap();
    }
    for dir in 0..130 {
        fs::create_dir_all(repo.path().join(format!("edge/d{dir:03}"))).unwrap();
        fs::write(
            repo.path().join(format!("edge/d{dir:03}/child.rs")),
            "// child",
        )
        .unwrap();
    }
    fs::create_dir_all(repo.path().join("wide/unopened/nested")).unwrap();
    let project = create_project(
        &mut fixture.client,
        "File Views",
        vec![repo.path().to_string_lossy().into_owned()],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut fixture.client, "bounded preview").await;
    assert!(bootstrap.file_list.roots[0].entries.len() <= 256);
    let root = project_root(&project, 0);
    let mut view = directory_view(&root, "wide");
    let mut seen = std::collections::BTreeSet::new();
    let mut first_next = None;
    loop {
        let ProjectFileViewState::Ready { entries, next, .. } =
            subscribe_file_view(&mut fixture.client, &project.id, view.clone()).await
        else {
            panic!("directory did not load");
        };
        assert!(entries.len() <= 128);
        for entry in entries
            .iter()
            .filter(|entry| entry.kind != protocol::ProjectFileKind::Directory)
        {
            assert!(
                seen.insert(entry.path.relative_path.clone()),
                "page repeated a file"
            );
        }
        if first_next.is_none() {
            first_next = next.clone();
        }
        let Some(cursor) = next else {
            break;
        };
        view = ProjectFileView::Directory {
            path: ProjectPath {
                root: ProjectRootPath(root.clone()),
                relative_path: "wide".to_owned(),
            },
            window: ProjectFileWindow::After(cursor),
            show_hidden: true,
            filter: String::new(),
        };
    }
    eprintln!("FILE VIEW FLOW: exhaustive pages settled");
    assert_eq!(
        seen.len(),
        FILES,
        "every file is reachable, not just the old 10k cap"
    );
    assert!(seen.contains("wide/f10049"));
    // Reverse navigation, including from a boundary whose file was deleted.
    let cursor = first_next.unwrap();
    fs::remove_file(repo.path().join(&cursor.path.relative_path)).unwrap();
    let previous = ProjectFileView::Directory {
        path: ProjectPath {
            root: ProjectRootPath(root.clone()),
            relative_path: "wide".to_owned(),
        },
        window: ProjectFileWindow::Before(cursor),
        show_hidden: true,
        filter: String::new(),
    };
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut fixture.client, &project.id, previous).await
    else {
        panic!("reverse page did not load");
    };
    assert_eq!(
        entries
            .iter()
            .find(|entry| entry.kind == protocol::ProjectFileKind::File)
            .unwrap()
            .path
            .relative_path,
        "wide/f00000"
    );

    eprintln!("FILE VIEW FLOW: reverse page settled");
    let ignored_view = ProjectFileView::Directory {
        path: ProjectPath {
            root: ProjectRootPath(root.clone()),
            relative_path: "wide".to_owned(),
        },
        window: ProjectFileWindow::First,
        show_hidden: true,
        filter: "zz-added".to_owned(),
    };
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut fixture.client, &project.id, ignored_view.clone()).await
    else {
        panic!("ignored view failed");
    };
    assert!(entries.is_empty());
    fs::write(repo.path().join("wide/zz-added.rs"), "// added").unwrap();
    let ProjectFileViewState::Ready { entries, .. } =
        file_view_state_matching(&mut fixture.client, &ignored_view, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path == "wide/zz-added.rs"))).await
    else {
        panic!("ignored view update failed");
    };
    assert_eq!(entries.len(), 1, "expanded ignored folders stay live");
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: ignored_view.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        file_view_state_matching(&mut fixture.client, &ignored_view, |state| *state
            == ProjectFileViewState::Closed)
        .await,
        ProjectFileViewState::Closed
    );
    eprintln!("FILE VIEW FLOW: ignored directory live update settled");
    let filtered_tree = ProjectFileView::Directory {
        path: ProjectPath {
            root: ProjectRootPath(root.clone()),
            relative_path: String::new(),
        },
        window: ProjectFileWindow::First,
        show_hidden: true,
        filter: "visible".to_owned(),
    };
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut fixture.client, &project.id, filtered_tree.clone()).await
    else {
        panic!("tree filter failed");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].path.relative_path, "deep",
        "filter preserves the path to matching descendants"
    );
    let (released, release_done) = tokio::sync::oneshot::channel();
    let filter_release_hook = fixture.on_project_scan(
        repo.path().join("wide/unopened/nested"),
        server::ScanPoint::WatcherUnregistered,
        move || {
            released.send(()).unwrap();
        },
    );
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: filtered_tree.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    file_view_state_matching(&mut fixture.client, &filtered_tree, |state| {
        *state == ProjectFileViewState::Closed
    })
    .await;
    eprintln!("FILE VIEW FLOW: waiting filtered scope native release");
    release_done.await.unwrap();
    drop(filter_release_hook);
    eprintln!("FILE VIEW FLOW: filtered scope released");
    for query in ["f10049", "d69/nested/visible"] {
        let view = ProjectFileView::QuickOpen {
            query: query.to_owned(),
            window: ProjectFileWindow::First,
        };
        let ProjectFileViewState::Ready { entries, .. } =
            subscribe_file_view(&mut fixture.client, &project.id, view.clone()).await
        else {
            panic!("quick-open failed");
        };
        assert_eq!(
            entries.len(),
            1,
            "quick-open searches unmaterialized and ignored directories"
        );
        let (released, release_done) = tokio::sync::oneshot::channel();
        let hook = fixture.on_project_scan(
            repo.path().join("wide/unopened/nested"),
            server::ScanPoint::WatcherUnregistered,
            move || {
                released.send(()).unwrap();
            },
        );
        fixture
            .client
            .project_file_view(
                &project.id,
                ProjectFileViewPayload {
                    view: view.clone(),
                    active: false,
                },
            )
            .await
            .unwrap();
        file_view_state_matching(&mut fixture.client, &view, |state| {
            *state == ProjectFileViewState::Closed
        })
        .await;
        release_done.await.unwrap();
        drop(hook);
    }
    eprintln!("FILE VIEW FLOW: quick-open scopes released");
    // Both connections must share the watcher actor being held below.
    let mut duplicate_reader = fixture.connect().await;
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let hook = fixture.on_project_scan(
        repo.path().join("wide/unopened/nested"),
        server::ScanPoint::WatcherRegistered,
        move || {
            entered.send(()).unwrap();
            held.recv().unwrap();
        },
    );
    let cancelled = ProjectFileView::QuickOpen {
        query: "cancelled-scope".into(),
        window: ProjectFileWindow::First,
    };
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: cancelled.clone(),
                active: true,
            },
        )
        .await
        .unwrap();
    eprintln!("FILE VIEW FLOW: waiting held native scope install");
    reached.await.unwrap();
    // Created after the scope's directory walk: neither read can rely on it.
    fs::create_dir(repo.path().join("wide/duplicate")).unwrap();
    let observed_file = repo.path().join("wide/duplicate/observed.rs");
    fs::write(&observed_file, "// observed").unwrap();
    let observed = ProjectPath {
        root: ProjectRootPath(root.clone()),
        relative_path: "wide/duplicate/observed.rs".into(),
    };
    let (first_wait, first_waiting) = watch_completion(
        &fixture,
        &observed_file,
        server::ScanPoint::FileObservationWaiting,
    );
    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: observed.clone(),
            },
        )
        .await
        .unwrap();
    first_waiting.await.unwrap();
    drop(first_wait);
    let (second_wait, mut second_waiting) = watch_completion(
        &fixture,
        &observed_file,
        server::ScanPoint::FileObservationWaiting,
    );
    duplicate_reader
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: observed.clone(),
            },
        )
        .await
        .unwrap();
    // Positive event or a real premature reply: no absence window or racing fence.
    let mut release = Some(release);
    loop {
        tokio::select! {
            waiting = &mut second_waiting => { waiting.unwrap(); break; }
            event = duplicate_reader.next_event() => {
                let event = event.unwrap().unwrap();
                if event.kind == FrameKind::ProjectFileContents {
                    release.take().unwrap().send(()).unwrap();
                    panic!("duplicate read succeeded before its native watch acknowledgement");
                }
                assert_ne!(event.kind, FrameKind::CommandError);
            }
        }
    }
    drop(second_wait);
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: cancelled.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    loop {
        let event = fixture.client.next_event().await.unwrap().unwrap();
        if event.kind == FrameKind::ProjectFileContents {
            release.take().unwrap().send(()).unwrap();
            panic!("read succeeded while its native watch worker was held");
        }
        if event.kind == FrameKind::ProjectFileViewState {
            let page: ProjectFileViewStatePayload = event.parse_payload().unwrap();
            if page.view == cancelled && page.state == ProjectFileViewState::Closed {
                break;
            }
        }
    }
    eprintln!("FILE VIEW FLOW: both reads await native ack; close delivered before worker release");
    release.take().unwrap().send(()).unwrap();
    let contents = expect_project_file_contents(
        &mut fixture.client,
        "read settles after cancelled watch registration",
    )
    .await;
    let duplicate = expect_project_file_contents(
        &mut duplicate_reader,
        "duplicate read settles after native observation",
    )
    .await;
    assert_eq!(contents.contents.as_deref(), Some("// observed"));
    assert_eq!(duplicate.contents.as_deref(), Some("// observed"));
    fs::write(&observed_file, "// changed").unwrap();
    for client in [&mut fixture.client, &mut duplicate_reader] {
        next_frame_matching_on(client, "both readers retain live ignored-file observation", |event| event.kind == FrameKind::ProjectEvent && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path == observed))).await;
    }
    drop(hook);
    eprintln!("FILE VIEW FLOW: read settled after native scope cancellation");
    let live_query = ProjectFileView::QuickOpen {
        query: "query-live-only".into(),
        window: ProjectFileWindow::First,
    };
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut fixture.client, &project.id, live_query.clone()).await
    else {
        panic!("initial live query failed");
    };
    assert!(entries.is_empty());
    fs::write(
        repo.path().join("wide/unopened/nested/query-live-only.rs"),
        "// live",
    )
    .unwrap();
    file_view_state_matching(&mut fixture.client, &live_query, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path == "wide/unopened/nested/query-live-only.rs"))).await;
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: live_query.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    file_view_state_matching(&mut fixture.client, &live_query, |state| {
        *state == ProjectFileViewState::Closed
    })
    .await;
    eprintln!("FILE VIEW FLOW: unchanged ignored query updated");
    let live_filter = ProjectFileView::Directory {
        path: ProjectPath {
            root: ProjectRootPath(root.clone()),
            relative_path: String::new(),
        },
        window: ProjectFileWindow::First,
        show_hidden: true,
        filter: "filter-live-only".into(),
    };
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut fixture.client, &project.id, live_filter.clone()).await
    else {
        panic!("initial live filter failed");
    };
    assert!(entries.is_empty());
    fs::write(
        repo.path().join("wide/unopened/nested/filter-live-only.rs"),
        "// live",
    )
    .unwrap();
    file_view_state_matching(&mut fixture.client, &live_filter, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path == "wide"))).await;

    eprintln!("FILE VIEW FLOW: unchanged ignored filter updated");
    let (unwatched, watch_released) = tokio::sync::oneshot::channel();
    let release_hook = fixture.on_project_scan(
        repo.path().join("wide/unopened/nested"),
        server::ScanPoint::WatcherUnregistered,
        move || {
            unwatched.send(()).unwrap();
        },
    );
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: live_filter.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    file_view_state_matching(&mut fixture.client, &live_filter, |state| {
        *state == ProjectFileViewState::Closed
    })
    .await;
    watch_released.await.unwrap();
    drop(release_hook);
    eprintln!("FILE VIEW FLOW: last live scope released");
    for dir in 0..70 {
        let view = directory_view(&root, &format!("deep/d{dir:02}/nested"));
        let ProjectFileViewState::Ready { entries, .. } =
            subscribe_file_view(&mut fixture.client, &project.id, view).await
        else {
            panic!("expanded directory failed");
        };
        assert_eq!(entries.len(), 1);
    }
    eprintln!("FILE VIEW FLOW: seventy views loaded");
    let first = directory_view(&root, "deep/d00/nested");
    fs::write(repo.path().join("deep/d00/nested/added.rs"), "// added").unwrap();
    let ProjectFileViewState::Ready { entries, .. } =
        file_view_state_matching(&mut fixture.client, &first, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path.ends_with("/added.rs")))).await
    else {
        panic!("retained directory failed");
    };
    assert_eq!(
        entries.len(),
        2,
        "opening >64 directories must not evict a visible view"
    );

    let failed = repo.path().join("deep/d00/nested");
    fs::remove_dir_all(&failed).unwrap();
    assert!(
        matches!(
            file_view_state_matching(&mut fixture.client, &first, |state| matches!(
                state,
                ProjectFileViewState::DirectoryMissing
            ))
            .await,
            ProjectFileViewState::DirectoryMissing
        ),
        "a deleted directory is explicit even without a pending request"
    );
    fs::create_dir(&failed).unwrap();
    fs::write(failed.join("restored.rs"), "// restored").unwrap();
    let ProjectFileViewState::Ready { entries, .. } =
        file_view_state_matching(&mut fixture.client, &first, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path.ends_with("/restored.rs")))).await
    else {
        panic!("retained failed view did not recover");
    };
    assert_eq!(entries[0].path.relative_path, "deep/d00/nested/restored.rs");

    eprintln!("FILE VIEW FLOW: deleted view recovered");
    let edge = directory_view(&root, "edge");
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut fixture.client, &project.id, edge.clone()).await
    else {
        panic!("edge page failed");
    };
    assert_eq!(entries.len(), 128);
    let expanded = directory_view(&root, "edge/d127");
    subscribe_file_view(&mut fixture.client, &project.id, expanded.clone()).await;
    fs::create_dir(repo.path().join("edge/a-new")).unwrap();
    let ProjectFileViewState::Ready { entries, .. } = file_view_state_matching(&mut fixture.client, &edge, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path == "edge/a-new"))).await else { panic!("edge update failed"); };
    assert!(
        entries
            .iter()
            .any(|entry| entry.path.relative_path == "edge/d127"),
        "live page shifts must retain the still-expanded boundary directory"
    );
    assert_eq!(
        entries.len(),
        129,
        "only declared active child rows extend the bounded page"
    );
    fs::write(repo.path().join("edge/d127/arrived.rs"), "// arrived").unwrap();
    file_view_state_matching(&mut fixture.client, &expanded, |state| matches!(state, ProjectFileViewState::Ready { entries, .. } if entries.iter().any(|entry| entry.path.relative_path.ends_with("arrived.rs")))).await;

    eprintln!("FILE VIEW FLOW: expanded edge retained");
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let held_hook = fixture.on_project_scan(
        repo.path().join("deep/d01/nested"),
        server::ScanPoint::ReadDirectory,
        move || {
            entered.send(()).unwrap();
            held.recv().unwrap();
        },
    );
    let metadata_view = ProjectFileView::Directory {
        path: ProjectPath {
            root: ProjectRootPath(root.clone()),
            relative_path: "deep/d01/nested".into(),
        },
        window: ProjectFileWindow::First,
        show_hidden: true,
        filter: "visible".into(),
    };
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: metadata_view.clone(),
                active: true,
            },
        )
        .await
        .unwrap();
    reached.await.unwrap();
    fixture
        .client
        .project_rename(ProjectRenamePayload {
            id: project.id.clone(),
            name: "Renamed file views".into(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut fixture.client, "metadata changed while view held", |event| event.kind == FrameKind::ProjectNotify && matches!(event.parse_payload::<ProjectNotifyPayload>().unwrap(), ProjectNotifyPayload::Upsert { project: changed } if changed.id == project.id && changed.name == "Renamed file views")).await;
    release.send(()).unwrap();
    let ProjectFileViewState::Ready { entries, .. } =
        file_view_state(&mut fixture.client, &metadata_view).await
    else {
        panic!("view stranded by metadata change");
    };
    assert_eq!(entries.len(), 1);
    drop(held_hook);

    // A fresh host has no hidden list of previously opened directories. Clients
    // re-declare the same views on replay, and receive real children again.
    eprintln!("FILE VIEW FLOW: catalog-generation read recovered");
    let mut restarted = fixture.connect_fresh_host().await;
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut restarted, &project.id, edge.clone()).await
    else {
        panic!("replayed parent failed");
    };
    assert!(
        !entries
            .iter()
            .any(|entry| entry.path.relative_path == "edge/d127")
    );
    restarted
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: expanded.clone(),
                active: true,
            },
        )
        .await
        .unwrap();
    let mut parent_retained = false;
    let mut child_restored = false;
    while !parent_retained || !child_restored {
        let event =
            next_frame_matching_on(&mut restarted, "replayed expanded page edge", |event| {
                event.kind == FrameKind::ProjectFileViewState
            })
            .await;
        let page: ProjectFileViewStatePayload = event.parse_payload().unwrap();
        if let ProjectFileViewState::Ready { entries, .. } = page.state {
            if page.view == edge {
                parent_retained |= entries
                    .iter()
                    .any(|entry| entry.path.relative_path == "edge/d127");
            }
            if page.view == expanded {
                child_restored |= entries
                    .iter()
                    .any(|entry| entry.path.relative_path == "edge/d127/child.rs");
            }
        }
    }
    eprintln!("FILE VIEW FLOW: reconnect restores off-page parent pin and child");
    let ProjectFileViewState::Ready { entries, .. } =
        subscribe_file_view(&mut restarted, &project.id, first.clone()).await
    else {
        panic!("redeclared view failed");
    };
    assert_eq!(entries.len(), 1);
    let routing_key = std::path::PathBuf::from(format!("/project/{}", project.id));
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let routing_hook = fixture.on_project_scan(
        routing_key.clone(),
        server::ScanPoint::FileViewRouting,
        move || {
            entered.send(()).unwrap();
            held.recv().unwrap();
        },
    );
    let (pressure_hook, mut pressure) = watch_completion(
        &fixture,
        &routing_key,
        server::ScanPoint::FileViewBackpressure,
    );
    restarted
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: first.clone(),
                active: true,
            },
        )
        .await
        .unwrap();
    reached.await.unwrap();
    for dir in 1..70 {
        restarted
            .project_file_view(
                &project.id,
                ProjectFileViewPayload {
                    view: directory_view(&root, &format!("deep/d{dir:02}/nested")),
                    active: true,
                },
            )
            .await
            .unwrap();
    }
    let mut release = Some(release);
    loop {
        tokio::select! {
            waiting = &mut pressure => {waiting.unwrap(); break;}
            event = restarted.next_event() => {
                let event = event.unwrap().unwrap();
                if event.kind == FrameKind::CommandError {
                    release.take().unwrap().send(()).unwrap();
                    panic!("replay declaration rejected instead of awaiting bounded capacity");
                }
            }
        }
    }
    release.take().unwrap().send(()).unwrap();
    drop(routing_hook);
    drop(pressure_hook);
    let mut received = std::collections::HashSet::new();
    while received.len() < 70 {
        let event = restarted.next_event().await.unwrap().unwrap();
        assert_ne!(
            event.kind,
            FrameKind::CommandError,
            "replay declarations must backpressure, never drop"
        );
        if event.kind == FrameKind::ProjectFileViewState {
            let page: ProjectFileViewStatePayload = event.parse_payload().unwrap();
            if let (
                ProjectFileView::Directory { path, .. },
                ProjectFileViewState::Ready { entries, .. },
            ) = (page.view, page.state)
                && path.relative_path.starts_with("deep/d")
                && path.relative_path.ends_with("/nested")
            {
                assert!(
                    entries
                        .iter()
                        .any(|entry| entry.path.relative_path.ends_with(".rs"))
                );
                received.insert(path);
            }
        }
    }
    eprintln!(
        "FILE VIEW FLOW: seventy replay declarations survive controlled control-lane backpressure"
    );
    // A refresh Ready may already be queued. Release completes on Closed, not
    // on whichever view event happens to be next in the stream.
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: first.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        file_view_state_matching(&mut fixture.client, &first, |state| *state
            == ProjectFileViewState::Closed)
        .await,
        ProjectFileViewState::Closed
    );
}

#[cfg(target_os = "linux")]
fn directory_watch_ids(path: &Path) -> Vec<(i32, i32)> {
    use std::os::unix::fs::MetadataExt;
    let inode = format!("ino:{:x}", fs::metadata(path).unwrap().ino());
    let mut watches = Vec::new();
    for entry in fs::read_dir("/proc/self/fdinfo")
        .unwrap()
        .filter_map(Result::ok)
    {
        let Ok(info) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in info.lines().filter(|line| line.starts_with("inotify ")) {
            if line.split_whitespace().any(|field| field == inode) {
                let wd = line
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("wd:"))
                    .unwrap();
                watches.push((
                    entry.file_name().to_str().unwrap().parse().unwrap(),
                    i32::from_str_radix(wd, 16).unwrap(),
                ));
            }
        }
    }
    watches.sort();
    watches
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn project_watch_failures_keep_other_projects_live() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut fixture = Fixture::new().await;
    let removed_repo = init_git_repo("stale-watch", &[("src/file.rs", "before\n")]);
    let healthy_repo = init_git_repo(
        "healthy-watch",
        &[("src/file.rs", "before\n"), ("src/unread.rs", "unread\n")],
    );
    let mut projects = Vec::new();
    for (name, repo) in [("Removed", &removed_repo), ("Healthy", &healthy_repo)] {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let ready = fixture.on_project_scan(
            repo.path().to_owned(),
            server::ScanPoint::WatcherReady,
            move || {
                let _ = ready_tx.send(());
            },
        );
        projects.push(
            create_project_with_real_roots(
                &mut fixture.client,
                name,
                vec![repo.path().to_string_lossy().into_owned()],
            )
            .await,
        );
        (ready_rx).await.unwrap();
        drop(ready);
    }
    let healthy_watches = directory_watch_ids(&healthy_repo.path().join("src"));
    assert_eq!(healthy_watches.len(), 1);
    let removed_path = removed_repo.path().join("src");
    let watches = directory_watch_ids(&removed_path);
    assert_eq!(watches.len(), 1);
    // Keep our own reference to the live inotify instance, then remove the
    // kernel watch without updating notify's map, as after a lost deletion.
    let duplicate = unsafe { libc::fcntl(watches[0].0, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(duplicate >= 0);
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    assert_eq!(
        unsafe { libc::inotify_rm_watch(duplicate.as_raw_fd(), watches[0].1) },
        0
    );
    drop(duplicate);
    let (failed_tx, failed_rx) = tokio::sync::oneshot::channel();
    let failure = fixture.on_project_scan(
        removed_path,
        server::ScanPoint::WatcherUnwatchFailed,
        move || {
            let _ = failed_tx.send(());
        },
    );
    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: projects[0].id.clone(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut fixture.client, "delete stale watch owner", |event| {
        if event.kind == FrameKind::CommandError {
            let error = event.parse_payload::<CommandErrorPayload>().expect("typed project deletion error");
            eprintln!("PROJECT DELETE unexpected error: operation={} fatal={}", error.operation, error.fatal);
        }
        assert_ne!(event.kind, FrameKind::CommandError);
        event.kind == FrameKind::ProjectNotify && matches!(event.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Delete { project }) if project.id == projects[0].id)
    }).await;
    (failed_rx).await.unwrap();
    drop(failure);
    fs::remove_dir_all(removed_repo.path()).unwrap();
    fs::write(healthy_repo.path().join("src/file.rs"), "still watching\n").unwrap();
    expect_watched_change(&mut fixture.client, "src/file.rs").await;
    assert_no_watched_changes(&mut fixture.client, &[]).await;
    assert_eq!(
        directory_watch_ids(&healthy_repo.path().join("src")),
        healthy_watches,
        "unrelated native registrations must survive another project's teardown error"
    );
    expect_inotify_instances(2).await;
    let storm_repo = init_git_repo(
        "kernel-overflow",
        &[("a.rs", "a\n"), ("b.rs", "b\n"), ("gone/leaf.rs", "gone\n")],
    );
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let ready = fixture.on_project_scan(
        storm_repo.path().to_owned(),
        server::ScanPoint::WatcherReady,
        move || {
            let _ = ready_tx.send(());
        },
    );
    let storm = create_project_with_real_roots(
        &mut fixture.client,
        "Kernel overflow",
        vec![storm_repo.path().to_string_lossy().into_owned()],
    )
    .await;
    (ready_rx).await.unwrap();
    drop(ready);
    fixture
        .client
        .project_read_file(
            &projects[1].id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: ProjectRootPath(project_root(&projects[1], 0)),
                    relative_path: "src/file.rs".to_owned(),
                },
            },
        )
        .await
        .unwrap();
    let before =
        expect_project_file_contents(&mut fixture.client, "observe before kernel overflow").await;
    // Catch-up rescans invalidate opened files, not every inventory entry.
    // Without this read, the old test waited for an unopened b.rs even when
    // the real watcher correctly reported observed_files=0 after queue loss.
    let storm_file = ProjectPath {
        root: ProjectRootPath(project_root(&storm, 0)),
        relative_path: "b.rs".to_owned(),
    };
    fixture
        .client
        .project_read_file(
            &storm.id,
            ProjectReadFilePayload {
                path: storm_file.clone(),
            },
        )
        .await
        .unwrap();
    let storm_before =
        expect_project_file_contents(&mut fixture.client, "observe before read flood").await;
    assert_eq!(storm_before.contents.as_deref(), Some("b\n"));
    let queue_capacity: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Read-only floods can overflow the shared kernel queue too, correctly
    // invalidating observed roots. Exercise actual lost writes, not load-
    // dependent absence/version assertions about a read-only flood.
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let paused = fixture.on_project_scan(
        storm_repo.path().join("a.rs"),
        server::ScanPoint::WatcherDispatch,
        move || {
            let _ = paused_tx.send(());
            release_rx
                .recv()
                .expect("release paused native event drain");
        },
    );
    let (overflow_tx, overflow_rx) = tokio::sync::oneshot::channel();
    let overflow = fixture.on_project_scan(
        healthy_repo.path().to_owned(),
        server::ScanPoint::WatcherKernelRescan,
        move || {
            let _ = overflow_tx.send(());
        },
    );
    fs::write(storm_repo.path().join("a.rs"), "pause\n").unwrap();
    (paused_rx).await.unwrap();
    let started = std::time::Instant::now();
    for index in 0..queue_capacity + 128 {
        fs::write(
            storm_repo
                .path()
                .join(if index % 2 == 0 { "a.rs" } else { "b.rs" }),
            "overflow\n",
        )
        .unwrap();
    }
    fs::remove_dir_all(storm_repo.path().join("gone")).unwrap();
    write_file(
        &storm_repo.path().join("new/leaf.rs"),
        "created while kernel queue full\n",
    );
    fs::write(
        healthy_repo.path().join("src/file.rs"),
        "changed while kernel queue full\n",
    )
    .unwrap();
    eprintln!(
        "kernel overflow: queue_capacity={queue_capacity} write_count={} elapsed_ms={}",
        queue_capacity + 128,
        started.elapsed().as_millis()
    );
    release_tx.send(()).unwrap();
    (overflow_rx)
        .await
        .expect("real native Q_OVERFLOW must be observed");
    drop(paused);
    drop(overflow);
    let mut changed = false;
    let mut listed = false;
    (async {
        while !changed || !listed {
            let event = fixture.client.next_event().await.unwrap().unwrap();
            assert_ne!(event.kind, FrameKind::CommandError);
            assert_ne!(event.kind, FrameKind::ProjectBootstrap);
            if event.stream == StreamPath(format!("/project/{}", projects[1].id))
                && event.kind == FrameKind::ProjectEvent
                && let protocol::ProjectEventPayload::FilesChanged { files } =
                    event.parse_payload().unwrap()
            {
                // A rescan cannot know what changed; reporting every visible
                // file produced frames larger than the whole output queue.
                assert!(
                    !files
                        .iter()
                        .any(|file| file.path.relative_path == "src/unread.rs"),
                    "rescan must report only files a client has read"
                );
                changed |= files
                    .iter()
                    .any(|file| file.path == before.path && file.version > before.version);
            }
            if event.stream == StreamPath(format!("/project/{}", storm.id))
                && event.kind == FrameKind::ProjectFileList
            {
                let listing: ProjectFileListPayload = event.parse_payload().unwrap();
                listed |= listing.roots[0]
                    .entries
                    .iter()
                    .any(|entry| entry.relative_path == "new/leaf.rs")
                    && !listing.roots[0]
                        .entries
                        .iter()
                        .any(|entry| entry.relative_path == "gone/leaf.rs");
            }
        }
    })
    .await;
    fs::write(
        storm_repo.path().join("new/leaf.rs"),
        "native watch restored\n",
    )
    .unwrap();
    expect_watched_change(&mut fixture.client, "new/leaf.rs").await;
    fixture
        .client
        .project_read_file(
            &projects[1].id,
            ProjectReadFilePayload { path: before.path },
        )
        .await
        .unwrap();
    let after =
        expect_project_file_contents(&mut fixture.client, "read after kernel overflow").await;
    assert_eq!(
        after.contents.as_deref(),
        Some("changed while kernel queue full\n")
    );
    assert!(after.version > before.version);
    expect_inotify_instances(2).await;
}

#[cfg(target_os = "linux")]
fn directory_has_inotify_watch(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let inode = format!(
        "ino:{:x}",
        fs::metadata(path)
            .expect("watched directory metadata")
            .ino()
    );
    fs::read_dir("/proc/self/fdinfo")
        .expect("inotify descriptors")
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .any(|info| {
            info.lines().any(|line| {
                line.starts_with("inotify ") && line.split_whitespace().any(|field| field == inode)
            })
        })
}

async fn expect_watched_change(client: &mut client::Connection, relative_path: &str) {
    expect_watched_change_at_phase(client, relative_path, "native change").await;
}

async fn expect_watched_change_at_phase(
    client: &mut client::Connection,
    relative_path: &str,
    phase: &'static str,
) {
    let mut frames = 0;
    let mut change_batches = 0;
    let mut changed_paths = 0;
    let mut listings = 0;
    let mut target_listed = false;
    eprintln!("PROJECT WATCH PHASE begin={phase}");
    // A local positive protocol wait has no timed product contract; nextest
    // owns hung-test protection, while disconnects and errors still fail.
    let started = std::time::Instant::now();
    (async {
        loop {
            let env = client
                .next_event()
                .await
                .expect("project event")
                .expect("open connection");
            frames += 1;
            assert_ne!(
                env.kind,
                FrameKind::CommandError,
                "watching failed: {env:?}"
            );
            match env.kind {
                FrameKind::ProjectEvent => {
                    if let protocol::ProjectEventPayload::FilesChanged { files } =
                        env.parse_payload().expect("file changes")
                    {
                        change_batches += 1;
                        changed_paths += files.len();
                        if files
                            .iter()
                            .any(|change| change.path.relative_path == relative_path)
                        {
                            return;
                        }
                    }
                }
                FrameKind::ProjectFileList => {
                    let listing: ProjectFileListPayload =
                        env.parse_payload().expect("watch phase file listing");
                    listings += 1;
                    target_listed |= listing.roots.iter().any(|root| {
                        root.entries.iter().any(|entry| {
                            entry.relative_path == relative_path && entry.op == FileEntryOp::Add
                        })
                    });
                }
                _ => {}
            }
        }
    })
    .await;
    eprintln!(
        "PROJECT WATCH PHASE end={phase} delivered=true frames={frames} change_batches={change_batches} changed_paths={changed_paths} listings={listings} target_listed={target_listed} elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

async fn assert_no_watched_changes(client: &mut client::Connection, paths: &[&str]) {
    let quiet = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            let env = client
                .next_event()
                .await
                .expect("project event")
                .expect("open connection");
            assert_ne!(
                env.kind,
                FrameKind::CommandError,
                "watching failed: {env:?}"
            );
            if env.kind == FrameKind::ProjectEvent
                && let protocol::ProjectEventPayload::FilesChanged { files } =
                    env.parse_payload().expect("file changes")
            {
                for change in files {
                    assert!(
                        !paths.contains(&change.path.relative_path.as_str()),
                        "ignored change escaped: {change:?}"
                    );
                }
            }
        }
    })
    .await;
    assert!(quiet.is_err());
}

#[cfg(target_os = "linux")]
async fn expect_inotify_instances(expected: usize) {
    let count = || {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                fs::read_link(entry.path())
                    .is_ok_and(|path| path == Path::new("anon_inode:inotify"))
            })
            .count()
    };
    (async {
        while count() != expected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn projects_share_native_watches_and_release_only_their_own_paths() {
    fixture::init_tracing();
    let mut fixture = Fixture::new().await;
    let mut repos = Vec::new();
    let mut projects = Vec::new();
    for index in 0..30 {
        let repo = init_git_repo(
            "shared-watches",
            &[("src/file.rs", "initial\n"), ("src/dir/leaf.rs", "leaf\n")],
        );
        let project = create_project_with_real_roots(
            &mut fixture.client,
            &format!("Shared Watch {index}"),
            vec![repo.path().to_string_lossy().into_owned()],
        )
        .await;
        repos.push(repo);
        projects.push(project);
    }
    let nested = create_project_with_real_roots(
        &mut fixture.client,
        "Nested Root",
        vec![repos[0].path().join("src").to_string_lossy().into_owned()],
    )
    .await;
    #[cfg(target_os = "linux")]
    expect_inotify_instances(2).await;

    let expected = [projects[0].clone(), nested.clone()];
    for project in &expected {
        let path = ProjectPath {
            root: ProjectRootPath(project_root(project, 0)),
            relative_path: if project.id == nested.id {
                "file.rs"
            } else {
                "src/file.rs"
            }
            .to_owned(),
        };
        fixture
            .client
            .project_read_file(&project.id, ProjectReadFilePayload { path })
            .await
            .unwrap();
        let event = next_frame_matching_on(&mut fixture.client, "observe shared file", |event| {
            event.kind == FrameKind::ProjectFileContents
                && event.stream == StreamPath(format!("/project/{}", project.id))
        })
        .await;
        assert_eq!(
            event
                .parse_payload::<ProjectFileContentsPayload>()
                .unwrap()
                .contents
                .as_deref(),
            Some("initial\n")
        );
    }
    fs::write(repos[0].path().join("src/file.rs"), "updated\n").unwrap();
    let mut remaining: std::collections::HashSet<_> = expected
        .iter()
        .map(|project| StreamPath(format!("/project/{}", project.id)))
        .collect();
    (async {
        while !remaining.is_empty() {
            let event = fixture.client.next_event().await.unwrap().unwrap();
            assert_ne!(event.kind, FrameKind::CommandError);
            if event.kind == FrameKind::ProjectEvent
                && let protocol::ProjectEventPayload::FilesChanged { files } =
                    event.parse_payload().unwrap()
                && files
                    .iter()
                    .any(|file| file.path.relative_path.ends_with("file.rs"))
            {
                let project = expected
                    .iter()
                    .find(|project| event.stream == StreamPath(format!("/project/{}", project.id)))
                    .expect("changes must not leak into unrelated projects");
                assert!(
                    files
                        .iter()
                        .any(|file| file.path.root.0 == project_root(project, 0)
                            && file.version.0 > 0)
                );
                remaining.remove(&event.stream);
            }
        }
    })
    .await;

    #[cfg(target_os = "linux")]
    {
        let healthy_registration = directory_watch_ids(&repos[1].path().join("src"));
        assert_eq!(healthy_registration.len(), 1);
        let slow_path = ProjectPath {
            root: ProjectRootPath(project_root(&projects[1], 0)),
            relative_path: "src/file.rs".to_owned(),
        };
        fixture
            .client
            .project_read_file(
                &projects[1].id,
                ProjectReadFilePayload {
                    path: slow_path.clone(),
                },
            )
            .await
            .unwrap();
        let before =
            expect_project_file_contents(&mut fixture.client, "observe slow project").await;
        let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked = fixture.on_project_scan(
            repos[1].path().to_owned(),
            server::ScanPoint::WatcherProcess,
            move || {
                let _ = blocked_tx.send(());
                release_rx.recv().expect("release stalled project worker");
            },
        );
        fs::write(repos[1].path().join("src/dir/leaf.rs"), "stall worker\n").unwrap();
        (blocked_rx).await.unwrap();
        let (overflow_tx, overflow_rx) = tokio::sync::oneshot::channel();
        let overflow = fixture.on_project_scan(
            repos[1].path().to_owned(),
            server::ScanPoint::WatcherOverflow,
            move || {
                let _ = overflow_tx.send(());
            },
        );
        for index in 0..300 {
            fs::write(
                repos[1].path().join(format!("src/burst-{index}.rs")),
                "burst\n",
            )
            .unwrap();
        }
        (overflow_rx).await.unwrap();
        fs::remove_dir_all(repos[1].path().join("src/dir")).unwrap();
        write_file(
            &repos[1].path().join("src/dir/leaf.rs"),
            "recreated during overflow\n",
        );
        fs::write(
            repos[1].path().join("src/file.rs"),
            "changed during overflow\n",
        )
        .unwrap();
        fs::write(repos[2].path().join("src/file.rs"), "healthy project\n").unwrap();
        next_frame_matching_on(&mut fixture.client, "healthy project while another worker is stalled", |event| {
        assert_ne!(event.kind, FrameKind::CommandError);
        event.stream == StreamPath(format!("/project/{}", projects[2].id))
            && event.kind == FrameKind::ProjectEvent
            && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path.relative_path == "src/file.rs"))
    }).await;
        release_tx.send(()).unwrap();
        drop(blocked);
        drop(overflow);
        next_frame_matching_on(&mut fixture.client, "overflow rescan invalidates observed file", |event| {
        assert_ne!(event.kind, FrameKind::CommandError);
        event.stream == StreamPath(format!("/project/{}", projects[1].id))
            && event.kind == FrameKind::ProjectEvent
            && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path == slow_path && file.version > before.version))
    }).await;
        fixture
            .client
            .project_read_file(&projects[1].id, ProjectReadFilePayload { path: slow_path })
            .await
            .unwrap();
        let recovered =
            expect_project_file_contents(&mut fixture.client, "overflow file contents").await;
        assert_eq!(
            recovered.contents.as_deref(),
            Some("changed during overflow\n")
        );
        assert!(recovered.version > before.version);
        assert_eq!(
            directory_watch_ids(&repos[1].path().join("src")),
            healthy_registration,
            "userspace overflow must retain healthy kernel registrations"
        );
        fs::write(
            repos[1].path().join("src/dir/leaf.rs"),
            "watch restored after dropped deletion\n",
        )
        .unwrap();
        expect_watched_change(&mut fixture.client, "src/dir/leaf.rs").await;
        fs::write(repos[1].path().join("src/burst-299.rs"), "after overflow\n").unwrap();
        expect_watched_change(&mut fixture.client, "src/burst-299.rs").await;
    }

    {
        let project = &projects[0];
        fixture
            .client
            .project_delete(ProjectDeletePayload {
                id: project.id.clone(),
            })
            .await
            .unwrap();
        next_frame_matching_on(&mut fixture.client, "delete shared owner", |event| {
            event.kind == FrameKind::ProjectNotify && matches!(event.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Delete { project: deleted }) if deleted.id == project.id)
        }).await;
    }
    fs::rename(
        repos[0].path().join("src/dir"),
        repos[0].path().join("src/renamed"),
    )
    .unwrap();
    next_frame_matching_on(
        &mut fixture.client,
        "remaining owner sees rename",
        |event| {
            event.stream == StreamPath(format!("/project/{}", nested.id))
                && event.kind == FrameKind::ProjectFileList
                && event
                    .parse_payload::<ProjectFileListPayload>()
                    .is_ok_and(|listing| {
                        listing.roots[0]
                            .entries
                            .iter()
                            .any(|entry| entry.relative_path == "renamed/leaf.rs")
                    })
        },
    )
    .await;
    fs::write(
        repos[0].path().join("src/renamed/leaf.rs"),
        "after rename\n",
    )
    .unwrap();
    next_frame_matching_on(&mut fixture.client, "remaining owner still watches renamed directory", |event| {
        event.stream == StreamPath(format!("/project/{}", nested.id)) && event.kind == FrameKind::ProjectEvent
            && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path.relative_path == "renamed/leaf.rs"))
    }).await;
    for project in std::iter::once(&nested).chain(projects[1..28].iter()) {
        fixture
            .client
            .project_delete(ProjectDeletePayload {
                id: project.id.clone(),
            })
            .await
            .unwrap();
        next_frame_matching_on(&mut fixture.client, "delete remaining watch owner", |event| {
            event.kind == FrameKind::ProjectNotify && matches!(event.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Delete { project: deleted }) if deleted.id == project.id)
        }).await;
    }
    let survivors = &projects[28..];
    let mut before_failure = std::collections::HashMap::new();
    for project in survivors {
        fixture
            .client
            .project_read_file(
                &project.id,
                ProjectReadFilePayload {
                    path: ProjectPath {
                        root: ProjectRootPath(project_root(project, 0)),
                        relative_path: "src/file.rs".to_owned(),
                    },
                },
            )
            .await
            .unwrap();
        let before =
            expect_project_file_contents(&mut fixture.client, "observe before shared failure")
                .await;
        before_failure.insert(
            StreamPath(format!("/project/{}", project.id)),
            before.version,
        );
    }
    let fault_indices = if cfg!(target_os = "linux") {
        vec![28]
    } else {
        vec![28, 29]
    };
    let faults: Vec<_> = fault_indices
        .iter()
        .map(|index| {
            fixture.on_project_scan(
                repos[*index].path().join("src/file.rs"),
                server::ScanPoint::WatcherFailure,
                || {},
            )
        })
        .collect();
    for index in fault_indices {
        fs::write(
            repos[index].path().join("src/file.rs"),
            "trigger native failure\n",
        )
        .unwrap();
    }
    let mut warned = std::collections::HashSet::new();
    (async {
        while warned.len() < survivors.len() {
            let event = fixture.client.next_event().await.unwrap().unwrap();
            if event.kind == FrameKind::CommandError {
                let error: CommandErrorPayload = event.parse_payload().unwrap();
                assert_eq!(error.operation, "project_watch");
                assert!(!error.fatal);
                assert!(error.message.contains("automatic recovery"));
                assert!(
                    error
                        .message
                        .contains("injected native filesystem watcher failure")
                );
                assert!(before_failure.contains_key(&event.stream));
                warned.insert(event.stream);
            }
        }
    })
    .await;
    drop(faults);
    for repo in &repos[28..] {
        fs::write(
            repo.path().join("src/file.rs"),
            "changed during native failure\n",
        )
        .unwrap();
    }
    let mut recovered = std::collections::HashSet::new();
    (async {
        while recovered.len() < survivors.len() {
            let event = fixture.client.next_event().await.unwrap().unwrap();
            assert_ne!(event.kind, FrameKind::ProjectBootstrap);
            assert_ne!(event.kind, FrameKind::CommandError);
            if event.kind == FrameKind::ProjectEvent
                && let protocol::ProjectEventPayload::FilesChanged { files } =
                    event.parse_payload().unwrap()
                && files.iter().any(|file| {
                    file.path.relative_path == "src/file.rs"
                        && file.version > before_failure[&event.stream]
                })
            {
                recovered.insert(event.stream);
            }
        }
    })
    .await;
    #[cfg(target_os = "linux")]
    expect_inotify_instances(2).await;
    for project in survivors {
        fixture
            .client
            .project_read_file(
                &project.id,
                ProjectReadFilePayload {
                    path: ProjectPath {
                        root: ProjectRootPath(project_root(project, 0)),
                        relative_path: "src/file.rs".to_owned(),
                    },
                },
            )
            .await
            .unwrap();
        let after =
            expect_project_file_contents(&mut fixture.client, "read after shared recovery").await;
        assert_eq!(
            after.contents.as_deref(),
            Some("changed during native failure\n")
        );
        assert!(after.version > before_failure[&StreamPath(format!("/project/{}", project.id))]);
        fs::write(
            Path::new(&project_root(project, 0)).join("src/file.rs"),
            "watching after recovery\n",
        )
        .unwrap();
        next_frame_matching_on(&mut fixture.client, "native events resume after shared recovery", |event| {
            event.stream == StreamPath(format!("/project/{}", project.id)) && event.kind == FrameKind::ProjectEvent
                && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path.relative_path == "src/file.rs" && file.version > after.version))
        }).await;
        fixture
            .client
            .project_delete(ProjectDeletePayload {
                id: project.id.clone(),
            })
            .await
            .unwrap();
        next_frame_matching_on(&mut fixture.client, "delete recovered watch owner", |event| {
            event.kind == FrameKind::ProjectNotify && matches!(event.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Delete { project: deleted }) if deleted.id == project.id)
        }).await;
    }
    #[cfg(target_os = "linux")]
    {
        expect_inotify_instances(1).await;
        assert!(!directory_has_inotify_watch(
            &repos[0].path().join("src/renamed")
        ));
    }
}

fn watch_completion(
    fixture: &Fixture,
    path: &Path,
    point: server::ScanPoint,
) -> (
    server::InstalledScanHook,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let hook = fixture.on_project_scan(path.to_owned(), point, move || {
        eprintln!("PROJECT WATCH COMPLETION point={point:?}");
        let _ = tx.send(());
    });
    (hook, rx)
}

async fn expect_watched_listing(client: &mut client::Connection, path: &str, present: bool) {
    eprintln!("PROJECT WATCH WAIT begin=listing fixture_entry={path} present={present}");
    let mut frames = 0;
    next_frame_matching_on(
        client,
        "watcher listing reflects filesystem state",
        |event| {
            frames += 1;
            eprintln!("PROJECT WATCH LISTING frame={frames} kind={:?}", event.kind);
            assert_ne!(event.kind, FrameKind::CommandError);
            event.kind == FrameKind::ProjectFileList && {
                let listing = event.parse_payload::<ProjectFileListPayload>().unwrap();
                !listing.incremental
                    && listing.roots.iter().any(|root| {
                        root.entries.iter().any(|entry| entry.relative_path == path) == present
                    })
            }
        },
    )
    .await;
    eprintln!(
        "PROJECT WATCH WAIT end=listing fixture_entry={path} present={present} frames={frames}"
    );
}

async fn expect_watched_write(
    client: &mut client::Connection,
    project: &Project,
    relative_path: &str,
    contents: &str,
) {
    let path = ProjectPath {
        root: ProjectRootPath(project_root(project, 0)),
        relative_path: relative_path.to_owned(),
    };
    let absolute = Path::new(&path.root.0).join(relative_path);
    eprintln!("PROJECT WATCH WAIT begin=observe-before-write fixture_entry={relative_path}");
    client
        .project_read_file(&project.id, ProjectReadFilePayload { path: path.clone() })
        .await
        .unwrap();
    let before = expect_project_file_contents(client, "observe file before watched write").await;
    eprintln!("PROJECT WATCH WAIT end=observe-before-write fixture_entry={relative_path}");
    fs::write(&absolute, contents).unwrap();
    eprintln!("PROJECT WATCH WAIT begin=invalidation fixture_entry={relative_path}");
    next_frame_matching_on(client, "watched file invalidation", |event| {
        assert_ne!(event.kind, FrameKind::CommandError);
        event.kind == FrameKind::ProjectEvent
            && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path == path && file.version > before.version))
    }).await;
    client
        .project_read_file(&project.id, ProjectReadFilePayload { path: path.clone() })
        .await
        .unwrap();
    eprintln!(
        "PROJECT WATCH WAIT end=invalidation begin=read-current fixture_entry={relative_path}"
    );
    let after = expect_project_file_contents(client, "read invalidated file").await;
    eprintln!("PROJECT WATCH WAIT end=read-current fixture_entry={relative_path}");
    assert_eq!(after.contents.as_deref(), Some(contents));
    assert!(after.version > before.version);
    eprintln!("PROJECT WATCH EVIDENCE: observed_file_invalidated=true contents_current=true");
}

#[tokio::test]
async fn project_watcher_prunes_ignored_build_trees() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "ignored-watches",
        &[
            ("src/main.rs", "initial\n"),
            ("tracked/keep.rs", "tracked\n"),
        ],
    );
    write_file(&repo.path().join(".gitignore"), "/target/\n/tracked/\n");
    write_file(
        &repo.path().join("target/other/deep/cache"),
        "unopened artifact",
    );
    write_file(
        &repo.path().join("target/debug/.fingerprint/cache"),
        "artifact",
    );
    write_file(&repo.path().join("pruned/deep/new.rs"), "unopened\n");
    let (ready, ready_rx) =
        watch_completion(&fixture, repo.path(), server::ScanPoint::WatcherReady);
    let project = create_project(
        &mut fixture.client,
        "Ignored watches",
        vec![repo.path().to_string_lossy().into_owned()],
    )
    .await;

    expect_project_bootstrap(&mut fixture.client, "watcher flow bootstrap").await;
    ready_rx.await.unwrap();
    drop(ready);
    #[cfg(target_os = "linux")]
    {
        for directory in [
            "target",
            "target/debug",
            "target/debug/.fingerprint",
            "target/other/deep",
        ] {
            assert!(
                !directory_has_inotify_watch(&repo.path().join(directory)),
                "ignored build directories must not consume OS watches: {directory}"
            );
        }
        assert!(
            !directory_has_inotify_watch(&repo.path().join(".git/objects")),
            "Git object storage must not consume OS watches"
        );
    }

    expect_watched_write(
        &mut fixture.client,
        &project,
        "src/main.rs",
        "source update\n",
    )
    .await;

    write_file(&repo.path().join("fresh/deep/new.rs"), "new\n");
    expect_watched_listing(&mut fixture.client, "fresh/deep/new.rs", true).await;
    expect_watched_write(
        &mut fixture.client,
        &project,
        "fresh/deep/new.rs",
        "second\n",
    )
    .await;
    let mut rename_diagnostics = Vec::new();
    for relative in ["fresh", "fresh/deep", "moved", "moved/deep"] {
        for point in [
            server::ScanPoint::WatcherDispatch,
            server::ScanPoint::WatcherReconcile,
            server::ScanPoint::WatcherProcessed,
            server::ScanPoint::WatcherRegistered,
            server::ScanPoint::WatcherUnregistered,
        ] {
            rename_diagnostics.push(fixture.on_project_scan(
                repo.path().join(relative),
                point,
                move || {
                    eprintln!("PROJECT WATCH RENAME fixture_entry={relative} point={point:?}");
                },
            ));
        }
    }
    // A listing is an independent filesystem snapshot, not watch readiness.
    // Complete the native rename without holding the worker during child creation.
    let (renamed, renamed_rx) = watch_completion(
        &fixture,
        &repo.path().join("moved"),
        server::ScanPoint::WatcherRenameProcessed,
    );
    eprintln!("PROJECT WATCH PHASE begin=rename-syscall");
    fs::rename(repo.path().join("fresh"), repo.path().join("moved")).unwrap();
    eprintln!("PROJECT WATCH PHASE end=rename-syscall");
    eprintln!("PROJECT WATCH WAIT begin=automatic-rename-completion");
    renamed_rx.await.unwrap();
    drop(renamed);
    eprintln!("PROJECT WATCH WAIT end=automatic-rename-completion");
    expect_watched_listing(&mut fixture.client, "moved/deep/new.rs", true).await;
    let unopened_child = repo.path().join("moved/deep/created-without-read.rs");
    let (automatic, automatic_rx) = watch_completion(
        &fixture,
        &unopened_child,
        server::ScanPoint::WatcherDispatch,
    );
    eprintln!("PROJECT WATCH PHASE begin=automatic-unopened-write");
    write_file(&unopened_child, "automatic renamed-directory watch\n");
    eprintln!("PROJECT WATCH WAIT begin=automatic-native-dispatch");
    automatic_rx.await.unwrap();
    eprintln!("PROJECT WATCH WAIT end=automatic-native-dispatch");
    expect_watched_listing(
        &mut fixture.client,
        "moved/deep/created-without-read.rs",
        true,
    )
    .await;
    drop(automatic);
    eprintln!(
        "PROJECT WATCH EVIDENCE: automatic renamed-directory native event and listing completed before any new-path read"
    );
    #[cfg(target_os = "linux")]
    {
        let moved_path = ProjectPath {
            root: ProjectRootPath(project_root(&project, 0)),
            relative_path: "moved/deep/new.rs".to_owned(),
        };
        let moved_file = repo.path().join("moved/deep/new.rs");
        // A dropped native event cannot identify an unopened file. The returned
        // read registers observation before a version-invalidation assertion;
        // unopened trees below retain their separate listing/pruning contract.
        fixture
            .client
            .project_read_file(
                &project.id,
                ProjectReadFilePayload {
                    path: moved_path.clone(),
                },
            )
            .await
            .unwrap();
        let before = expect_project_file_contents(
            &mut fixture.client,
            "observe renamed file before queue loss",
        )
        .await;
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let paused = fixture.on_project_scan(
            repo.path().to_owned(),
            server::ScanPoint::WatcherProcess,
            move || {
                let _ = paused_tx.send(());
                release_rx.recv().expect("release rename catch-up worker");
            },
        );
        fs::write(repo.path().join("src/main.rs"), "hold after rename\n").unwrap();
        paused_rx.await.unwrap();
        let invalidated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invalidated_copy = invalidated.clone();
        let target = fixture.on_project_scan(
            moved_file.clone(),
            server::ScanPoint::WatcherInvalidated,
            move || {
                invalidated_copy.store(true, std::sync::atomic::Ordering::SeqCst);
            },
        );
        let unopened_invalidated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let unopened_copy = unopened_invalidated.clone();
        let unopened = fixture.on_project_scan(
            repo.path().join("pruned/deep/new.rs"),
            server::ScanPoint::WatcherInvalidated,
            move || {
                unopened_copy.store(true, std::sync::atomic::Ordering::SeqCst);
            },
        );
        let (rescanned, rescanned_rx) = watch_completion(
            &fixture,
            repo.path(),
            server::ScanPoint::WatcherRescanFinished,
        );
        let (dropped, dropped_rx) =
            watch_completion(&fixture, &moved_file, server::ScanPoint::WatcherDropped);
        // More distinct native events than the 128-entry queue, with its worker
        // held. Alternating paths avoids kernel coalescing without a file storm.
        for index in 0..129 {
            fs::write(
                repo.path().join(format!("src/queue-{}.rs", index % 2)),
                "queue pressure\n",
            )
            .unwrap();
        }
        fs::write(&moved_file, "after move\n").unwrap();
        dropped_rx.await.unwrap();
        eprintln!(
            "PROJECT WATCH EVIDENCE: renamed-file native event explicitly dropped while worker held; target observation completed before write"
        );
        release_tx.send(()).unwrap();
        rescanned_rx.await.unwrap();
        eprintln!(
            "PROJECT WATCH EVIDENCE: catch-up finished observed_target_invalidated={} unopened_target_invalidated={}",
            invalidated.load(std::sync::atomic::Ordering::SeqCst),
            unopened_invalidated.load(std::sync::atomic::Ordering::SeqCst)
        );
        drop((paused, dropped, target, unopened, rescanned));
        next_frame_matching_on(&mut fixture.client, "observed renamed-file version after explicit queue loss", |event| {
        assert_ne!(event.kind, FrameKind::CommandError);
        event.kind == FrameKind::ProjectEvent
            && matches!(event.parse_payload::<protocol::ProjectEventPayload>(), Ok(protocol::ProjectEventPayload::FilesChanged { files }) if files.iter().any(|file| file.path == moved_path && file.version > before.version))
    }).await;
        fixture
            .client
            .project_read_file(
                &project.id,
                ProjectReadFilePayload {
                    path: moved_path.clone(),
                },
            )
            .await
            .unwrap();
        let after = expect_project_file_contents(
            &mut fixture.client,
            "renamed-file contents after explicit queue loss",
        )
        .await;
        assert_eq!(after.contents.as_deref(), Some("after move\n"));
        assert!(after.version > before.version);
    }
    expect_watched_write(
        &mut fixture.client,
        &project,
        "moved/deep/new.rs",
        "native watch restored\n",
    )
    .await;

    write_file(&repo.path().join("shape"), "was a file\n");
    expect_watched_listing(&mut fixture.client, "shape", true).await;
    fs::remove_file(repo.path().join("shape")).unwrap();
    write_file(&repo.path().join("shape/nested.rs"), "now a directory\n");
    expect_watched_listing(&mut fixture.client, "shape/nested.rs", true).await;
    expect_watched_write(
        &mut fixture.client,
        &project,
        "shape/nested.rs",
        "directory contents changed\n",
    )
    .await;
    fs::remove_dir_all(repo.path().join("shape")).unwrap();
    expect_watched_listing(&mut fixture.client, "shape/nested.rs", false).await;
    write_file(
        &repo.path().join("shape/nested.rs"),
        "recreated directory\n",
    );
    expect_watched_listing(&mut fixture.client, "shape/nested.rs", true).await;
    expect_watched_write(
        &mut fixture.client,
        &project,
        "shape/nested.rs",
        "recreated contents changed\n",
    )
    .await;

    write_file(
        &repo.path().join(".settings/editor.toml"),
        "enabled = true\n",
    );
    expect_watched_listing(&mut fixture.client, ".settings/editor.toml", true).await;
    expect_watched_write(
        &mut fixture.client,
        &project,
        "tracked/keep.rs",
        "tracked update\n",
    )
    .await;

    let pruned = repo.path().join("pruned/deep");
    for (control, ignored) in [
        (
            repo.path().join(".gitignore"),
            "/target/\n/tracked/\n/pruned/\n",
        ),
        (repo.path().join("pruned/.gitignore"), "deep/\n"),
        (repo.path().join(".git/info/exclude"), "pruned/\n"),
    ] {
        let (removed, removed_rx) =
            watch_completion(&fixture, &pruned, server::ScanPoint::WatcherUnregistered);
        fs::write(&control, ignored).unwrap();
        removed_rx.await.unwrap();
        // Pruning itself invalidates entries removed from the watch inventory.
        // Consume that transition before asserting absence for later writes.
        expect_watched_change_at_phase(
            &mut fixture.client,
            "pruned/deep/new.rs",
            "ignore policy adopted",
        )
        .await;
        #[cfg(target_os = "linux")]
        assert!(
            !directory_has_inotify_watch(&pruned),
            "ignore rules must release unopened subtree watches"
        );
        fs::write(pruned.join("new.rs"), "ignored update\n").unwrap();
        fs::write(
            repo.path().join("target/debug/.fingerprint/cache"),
            "new artifact",
        )
        .unwrap();
        assert_no_watched_changes(
            &mut fixture.client,
            &["pruned/deep/new.rs", "target/debug/.fingerprint/cache"],
        )
        .await;

        let (restored, restored_rx) =
            watch_completion(&fixture, &pruned, server::ScanPoint::WatcherRegistered);
        if control == repo.path().join(".gitignore") {
            fs::write(&control, "/target/\n/tracked/\n").unwrap();
        } else {
            fs::remove_file(&control).unwrap();
        }
        restored_rx.await.unwrap();
        expect_watched_change_at_phase(
            &mut fixture.client,
            "pruned/deep/new.rs",
            "ignore policy removed",
        )
        .await;
        #[cfg(target_os = "linux")]
        assert!(
            directory_has_inotify_watch(&pruned),
            "restored unopened subtree must regain its native watch"
        );
        write_file(&pruned.join("created-after-restore.rs"), "watch restored\n");
        expect_watched_listing(
            &mut fixture.client,
            "pruned/deep/created-after-restore.rs",
            true,
        )
        .await;
        fs::remove_file(pruned.join("created-after-restore.rs")).unwrap();
        expect_watched_listing(
            &mut fixture.client,
            "pruned/deep/created-after-restore.rs",
            false,
        )
        .await;
        drop((removed, restored));
    }

    // A real listing can see a file before its queued native create is classified.
    // Hold the watcher, not the client or actor, across that visible file's deletion.
    expect_watched_write(
        &mut fixture.client,
        &project,
        "src/main.rs",
        "before queued create race\n",
    )
    .await;
    let (scan_tx, scan_rx) = tokio::sync::oneshot::channel();
    let (scan_release_tx, scan_release_rx) = std::sync::mpsc::channel();
    let scan = fixture.on_project_scan(
        pruned.clone(),
        server::ScanPoint::ReadDirectory,
        move || {
            scan_tx.send(()).unwrap();
            scan_release_rx.recv().unwrap();
        },
    );
    fs::write(
        repo.path().join("src/main.rs"),
        "start independent listing\n",
    )
    .unwrap();
    scan_rx.await.unwrap();
    let race_file = pruned.join("created-before-dispatch.rs");
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held = fixture.on_project_scan(
        race_file.clone(),
        server::ScanPoint::WatcherProcess,
        move || {
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        },
    );
    fs::write(&race_file, "visible before watcher classification\n").unwrap();
    held_rx.await.unwrap();
    let (settled, settled_rx) =
        watch_completion(&fixture, repo.path(), server::ScanPoint::FileRefreshSettled);
    scan_release_tx.send(()).unwrap();
    expect_watched_listing(
        &mut fixture.client,
        "pruned/deep/created-before-dispatch.rs",
        true,
    )
    .await;
    settled_rx.await.unwrap();
    fs::remove_file(&race_file).unwrap();
    eprintln!(
        "PROJECT WATCH RACE EVIDENCE file_listed=true prior_refreshes_settled=true removed_before_create_classified=true"
    );
    release_tx.send(()).unwrap();
    expect_watched_listing(
        &mut fixture.client,
        "pruned/deep/created-before-dispatch.rs",
        false,
    )
    .await;
    drop((scan, held, settled));

    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: ProjectPath {
                    root: ProjectRootPath(project_root(&project, 0)),
                    relative_path: "target/debug/.fingerprint/cache".to_owned(),
                },
            },
        )
        .await
        .expect("explicitly open ignored file");
    let initial = expect_project_file_contents(&mut fixture.client, "ignored file contents").await;
    write_file(
        &repo.path().join("target/debug/.fingerprint/replacement"),
        "opened update",
    );
    fs::rename(
        repo.path().join("target/debug/.fingerprint/replacement"),
        repo.path().join("target/debug/.fingerprint/cache"),
    )
    .unwrap();
    expect_watched_change(&mut fixture.client, "target/debug/.fingerprint/cache").await;
    fixture
        .client
        .project_read_file(&project.id, ProjectReadFilePayload { path: initial.path })
        .await
        .unwrap();
    let updated = expect_project_file_contents(&mut fixture.client, "updated ignored file").await;
    assert_eq!(updated.contents.as_deref(), Some("opened update"));
    assert!(updated.version > initial.version);
    #[cfg(target_os = "linux")]
    assert!(
        !directory_has_inotify_watch(&repo.path().join("target/other/deep")),
        "opening one ignored file must not watch sibling build trees"
    );
}

#[tokio::test]
async fn project_watcher_follows_worktree_git_metadata() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("worktree-watches", &[("source.rs", "initial\n")]);
    let checkout = tempfile::tempdir().unwrap();
    let worktree = checkout.path().join("checkout");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            "watch-branch",
            worktree.to_str().unwrap(),
        ],
    );
    write_file(&worktree.join(".gitignore"), "target/\n");
    write_file(&worktree.join("target/deep/cache"), "artifact");
    let claim_ref = "refs/tyde/workbench-creation/watcher-fixture";
    git(repo.path(), &["update-ref", claim_ref, "HEAD"]);
    let claim_dir = repo.path().join(".git/refs/tyde/workbench-creation");
    let (removed_tx, removed_rx) = tokio::sync::oneshot::channel();
    let removal_repo = repo.path().to_owned();
    // Git removes the empty ownership-ref directory between stat and OS watch.
    let removal = fixture.on_project_scan(
        claim_dir.clone(),
        server::ScanPoint::WatcherRegisterPending,
        move || {
            git(&removal_repo, &["update-ref", "-d", claim_ref]);
            removed_tx.send(()).unwrap();
        },
    );
    let (primary_ready_tx, primary_ready_rx) = tokio::sync::oneshot::channel();
    let primary_ready = fixture.on_project_scan(
        repo.path().to_owned(),
        server::ScanPoint::WatcherReady,
        move || {
            let _ = primary_ready_tx.send(());
        },
    );
    let primary = create_project(
        &mut fixture.client,
        "Primary shared Git watches",
        vec![repo.path().to_string_lossy().into_owned()],
    )
    .await;
    expect_project_bootstrap(&mut fixture.client, "primary bootstrap").await;
    removed_rx.await.expect("actual ownership ref removed");
    primary_ready_rx.await.unwrap();
    assert!(!claim_dir.exists());
    drop((removal, primary_ready));
    let (registered, registered_rx) =
        watch_completion(&fixture, &claim_dir, server::ScanPoint::WatcherRegistered);
    git(repo.path(), &["update-ref", claim_ref, "HEAD"]);
    registered_rx
        .await
        .expect("recreated ref directory watched");
    drop(registered);
    git(repo.path(), &["update-ref", "-d", claim_ref]);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let ready = fixture.on_project_scan(
        worktree.clone(),
        server::ScanPoint::WatcherReady,
        move || {
            let _ = ready_tx.send(());
        },
    );
    let project = create_project(
        &mut fixture.client,
        "Worktree watches",
        vec![worktree.to_string_lossy().into_owned()],
    )
    .await;
    expect_project_bootstrap(&mut fixture.client, "worktree bootstrap").await;
    (ready_rx).await.unwrap();
    drop(ready);
    #[cfg(target_os = "linux")]
    expect_inotify_instances(2).await;
    fixture
        .client
        .project_delete(ProjectDeletePayload {
            id: primary.id.clone(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut fixture.client, "remove shared Git owner", |event| {
        event.kind == FrameKind::ProjectNotify && matches!(event.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Delete { project }) if project.id == primary.id)
    }).await;
    fs::write(worktree.join("source.rs"), "modified\n").unwrap();
    expect_watched_change(&mut fixture.client, "source.rs").await;
    let root = ProjectRootPath(project_root(&project, 0));
    let git_dir =
        std::path::PathBuf::from(git_stdout(&worktree, &["rev-parse", "--absolute-git-dir"]));
    let index_processed = fixture.on_project_scan(
        git_dir.join("index"),
        server::ScanPoint::WatcherProcessed,
        || eprintln!("WORKTREE_METADATA phase=index native_processed=true"),
    );
    eprintln!("WORKTREE_METADATA phase=index command=begin");
    git(&worktree, &["add", "source.rs"]);
    eprintln!("WORKTREE_METADATA phase=index command=complete protocol_wait=begin");
    (expect_project_git_status_matching(&mut fixture.client, "worktree index update", |status| {
        let staged = status.roots.iter().any(|entry| {
            entry.root == root
                && entry
                    .files
                    .iter()
                    .any(|file| file.relative_path == "source.rs" && file.staged.is_some())
        });
        eprintln!(
            "WORKTREE_METADATA phase=index protocol_staged={staged} roots={}",
            status.roots.len()
        );
        staged
    }))
    .await;
    eprintln!("WORKTREE_METADATA phase=index protocol_wait=complete");
    drop(index_processed);
    let head_processed = fixture.on_project_scan(
        git_dir.join("HEAD"),
        server::ScanPoint::WatcherProcessed,
        || eprintln!("WORKTREE_METADATA phase=head native_processed=true"),
    );
    eprintln!("WORKTREE_METADATA phase=head command=begin");
    git(&worktree, &["branch", "-m", "renamed-watch-branch"]);
    eprintln!("WORKTREE_METADATA phase=head command=complete protocol_wait=begin");
    (expect_project_git_status_matching(&mut fixture.client, "worktree HEAD update", |status| {
        let renamed = status.roots[0].branch.as_deref() == Some("renamed-watch-branch");
        eprintln!(
            "WORKTREE_METADATA phase=head protocol_renamed={renamed} roots={}",
            status.roots.len()
        );
        renamed
    }))
    .await;
    eprintln!("WORKTREE_METADATA phase=head protocol_wait=complete");
    drop(head_processed);
    let commit_processed = fixture.on_project_scan(
        git_dir.join("HEAD"),
        server::ScanPoint::WatcherProcessed,
        || eprintln!("WORKTREE_METADATA phase=commit native_processed=true"),
    );
    let old_head = git_stdout(&worktree, &["rev-parse", "HEAD"]);
    eprintln!("WORKTREE_METADATA phase=commit command=begin");
    git(
        &worktree,
        &["commit", "-m", "Advance staged source for ref watches"],
    );
    eprintln!("WORKTREE_METADATA phase=commit command=complete protocol_wait=begin");
    let new_head = git_stdout(&worktree, &["rev-parse", "HEAD"]);
    assert_ne!(old_head, new_head, "staged source must advance HEAD");
    expect_project_git_status_matching(
        &mut fixture.client,
        "committed worktree source",
        |status| {
            let advanced = status.roots.iter().any(|entry| {
                entry.root == root && entry.head_oid.as_deref() == Some(new_head.as_str())
            });
            eprintln!(
                "WORKTREE_METADATA phase=commit protocol_advanced={advanced} roots={}",
                status.roots.len()
            );
            advanced
        },
    )
    .await;
    eprintln!("WORKTREE_METADATA phase=commit protocol_wait=complete");
    drop(commit_processed);
    git(
        &worktree,
        &["config", "branch.renamed-watch-branch.remote", "."],
    );
    for leaf in ["flat", "nested/deep/upstream"] {
        let reference = format!("refs/heads/{leaf}");
        git(
            &worktree,
            &["config", "branch.renamed-watch-branch.merge", &reference],
        );
        for (phase, target, ahead) in [
            ("create", Some(old_head.as_str()), 1),
            ("advance", Some(new_head.as_str()), 0),
            ("rewind", Some(old_head.as_str()), 1),
            ("delete", None, 0),
        ] {
            // The failed run awaited a ref acknowledgement through overflow recovery.
            // Hold a real queue full to prove protocol completion does not need
            // an individual native-path acknowledgement.
            #[cfg(target_os = "linux")]
            let overflow_case = if leaf == "flat" && phase == "create" {
                let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let blocked = fixture.on_project_scan(
                    worktree.clone(),
                    server::ScanPoint::WatcherProcess,
                    move || {
                        let _ = blocked_tx.send(());
                        release_rx.recv().expect("release held worktree watcher");
                    },
                );
                fs::write(worktree.join("source.rs"), "modified\n").unwrap();
                blocked_rx.await.expect("real worktree worker held");
                let (overflow_tx, overflow_rx) = tokio::sync::oneshot::channel();
                let overflow = fixture.on_project_scan(
                    worktree.join("burst-299.rs"),
                    server::ScanPoint::WatcherDropped,
                    move || {
                        let _ = overflow_tx.send(());
                    },
                );
                for index in 0..300 {
                    fs::write(worktree.join(format!("burst-{index}.rs")), "burst\n").unwrap();
                }
                overflow_rx.await.expect("real worktree queue overflowed");
                eprintln!("WORKTREE_METADATA phase=ref queue_overflow=observed");
                let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
                let dropped = fixture.on_project_scan(
                    repo.path().join(".git").join(&reference),
                    server::ScanPoint::WatcherDropped,
                    move || {
                        let _ = dropped_tx.send(());
                    },
                );
                Some((blocked, overflow, dropped, release_tx, dropped_rx))
            } else {
                None
            };
            eprintln!(
                "WORKTREE_METADATA phase=ref operation={phase} nested={} command=begin",
                leaf != "flat"
            );
            match target {
                Some(target) => git(&worktree, &["update-ref", &reference, target]),
                None => git(&worktree, &["update-ref", "-d", &reference]),
            }
            eprintln!("WORKTREE_METADATA phase=ref operation={phase} command=complete");
            #[cfg(target_os = "linux")]
            if let Some((blocked, overflow, dropped, release_tx, dropped_rx)) = overflow_case {
                dropped_rx
                    .await
                    .expect("actual ref event dropped by the full queue");
                eprintln!("WORKTREE_METADATA phase=ref operation={phase} native_dropped=observed");
                for index in 0..300 {
                    fs::remove_file(worktree.join(format!("burst-{index}.rs"))).unwrap();
                }
                release_tx
                    .send(())
                    .expect("release overflowing worktree worker");
                drop((blocked, overflow, dropped));
            }
            eprintln!(
                "WORKTREE_METADATA phase=ref operation={phase} protocol_wait=begin expected_ahead={ahead}"
            );
            let status = expect_project_git_status_matching(
                &mut fixture.client,
                "shared upstream ref change",
                |status| {
                    let matched = status.roots.iter().any(|entry| {
                        entry.root == root
                            && entry.branch.as_deref() == Some("renamed-watch-branch")
                            && entry.head_oid.as_deref() == Some(new_head.as_str())
                            && entry.ahead == ahead
                            && entry.behind == 0
                    });
                    eprintln!("WORKTREE_METADATA phase=ref operation={phase} protocol_matched={matched} roots={}", status.roots.len());
                    matched
                },
            )
            .await;
            eprintln!("WORKTREE_METADATA phase=ref operation={phase} protocol_wait=complete");
            assert_eq!(status.roots.len(), 1, "exact linked-worktree status root");
        }
    }
    #[cfg(target_os = "linux")]
    {
        assert!(!directory_has_inotify_watch(&worktree.join("target/deep")));
        assert!(!directory_has_inotify_watch(
            &repo.path().join(".git/objects")
        ));
    }
}

#[tokio::test]
async fn live_watcher_refreshes_files_during_continuous_activity() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "delete-poll",
        &[("keep.rs", "// keep\n"), ("remove_me.rs", "// remove\n")],
    );

    let project = create_project_with_real_roots(
        &mut fixture.client,
        "Delete Poll",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;

    let changed_path = ProjectPath {
        root: ProjectRootPath(project_root(&project, 0)),
        relative_path: "keep.rs".to_owned(),
    };
    fixture
        .client
        .project_read_file(
            &project.id,
            ProjectReadFilePayload {
                path: changed_path.clone(),
            },
        )
        .await
        .expect("read initial file");
    let initial = expect_project_file_contents(&mut fixture.client, "initial contents").await;

    // Delete real paths at the two scan race boundaries, not before a scan starts.
    for (relative, point, directory) in [
        ("vanishing.tmp", server::ScanPoint::Metadata, false),
        ("vanishing-dir", server::ScanPoint::ReadDirectory, true),
    ] {
        let path = repo.path().join(relative);
        let remove_path = path.clone();
        let (removed_tx, removed_rx) = tokio::sync::oneshot::channel();
        let hook = fixture.on_project_scan(path.clone(), point, move || {
            if directory {
                fs::remove_dir_all(&remove_path).expect("delete directory during scan");
            } else {
                fs::remove_file(&remove_path).expect("delete file during scan");
            }
            eprintln!("removed {} during project scan", remove_path.display());
            removed_tx.send(()).expect("scan observer still waiting");
        });
        if directory {
            fs::create_dir(&path).expect("create transient directory");
        } else {
            fs::write(&path, "temporary").expect("create transient file");
        }
        (removed_rx).await.expect("scan hook must delete the path");
        let marker = format!("survived-{relative}");
        fs::write(repo.path().join(&marker), "still watching").expect("write scan marker");
        let listing = expect_project_file_list_matching(
            &mut fixture.client,
            "live file listing after a path disappears during scan",
            |listing| {
                listing.roots[0]
                    .entries
                    .iter()
                    .any(|entry| entry.relative_path == marker)
            },
        )
        .await;
        assert!(
            listing.roots[0].entries.iter().all(|entry| {
                entry.relative_path != relative
                    && !entry.relative_path.starts_with(&format!("{relative}/"))
            }),
            "deleted paths must not remain in the listing: {listing:?}"
        );
        drop(hook);
    }

    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: directory_view(&project_root(&project, 0), "vanishing-dir"),
                active: true,
            },
        )
        .await
        .expect("request deleted directory");
    // The fixture deleted this directory: NotFound is the typed deletion state,
    // not a transient read error. Keep requiring that exact explicit outcome.
    assert!(matches!(
        file_view_state(
            &mut fixture.client,
            &directory_view(&project_root(&project, 0), "vanishing-dir")
        )
        .await,
        ProjectFileViewState::DirectoryMissing
    ));
    let deleted_view = directory_view(&project_root(&project, 0), "vanishing-dir");
    fixture
        .client
        .project_file_view(
            &project.id,
            ProjectFileViewPayload {
                view: deleted_view.clone(),
                active: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        file_view_state_matching(&mut fixture.client, &deleted_view, |state| *state
            == ProjectFileViewState::Closed)
        .await,
        ProjectFileViewState::Closed
    ));

    fs::remove_file(repo.path().join("remove_me.rs")).expect("failed to delete remove_me.rs");
    fs::write(repo.path().join("keep.rs"), "// updated\n").expect("update keep.rs");

    // Builds and agents need not leave the entire project idle for a refresh.
    let activity_path = repo.path().join("activity.log");
    let (mut activity_client, _) = fixture.connect_with_bootstrap().await;
    let activity = async {
        let mut writes = 0;
        loop {
            fs::write(&activity_path, format!("activity {writes}\n")).expect("write activity");
            fs::read(&activity_path).expect("read activity");
            writes += 1;
            activity_client
                .heartbeat(protocol::HeartbeatPayload {
                    client_sent_at_ms: writes,
                })
                .await
                .unwrap();
            next_frame_matching_on(
                &mut activity_client,
                "activity command completed",
                |event| event.kind == FrameKind::HeartbeatAck,
            )
            .await;
        }
    };
    let update_started = std::time::Instant::now();
    let receive_updates = async {
        let mut listing = None;
        let mut version = None;
        while listing.is_none() || version.is_none() {
            let env = fixture
                .client
                .next_event()
                .await
                .expect("project event")
                .expect("connection open");
            eprintln!(
                "continuous watcher update after {:?}: kind={:?} payload={}",
                update_started.elapsed(),
                env.kind,
                env.payload
            );
            match env.kind {
                FrameKind::ProjectFileList => {
                    let files: ProjectFileListPayload = env.parse_payload().expect("file list");
                    if files.roots[0]
                        .entries
                        .iter()
                        .all(|entry| entry.relative_path != "remove_me.rs")
                    {
                        listing = Some(files);
                    }
                }
                FrameKind::ProjectEvent => {
                    if let protocol::ProjectEventPayload::FilesChanged { files } =
                        env.parse_payload().expect("file changes")
                    {
                        for change in files {
                            if change.path == changed_path && change.version > initial.version {
                                version = Some(change.version);
                            }
                        }
                    }
                }
                FrameKind::CommandError => panic!("normal filesystem activity failed: {env:?}"),
                _ => {}
            }
        }
        (
            listing.expect("updated listing"),
            version.expect("updated version"),
        )
    };
    let (file_list, changed_version) = tokio::select! {
        result = receive_updates => result,
        () = activity => unreachable!("activity continues until updates arrive"),
    };
    fixture
        .client
        .project_read_file(&project.id, ProjectReadFilePayload { path: changed_path })
        .await
        .expect("read changed file");
    let updated = expect_project_file_contents(&mut fixture.client, "updated contents").await;
    assert_eq!(updated.contents.as_deref(), Some("// updated\n"));
    assert!(updated.version >= changed_version);
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

    let root = repo.path().to_path_buf();
    let remove_root = root.clone();
    let (removed_tx, removed_rx) = tokio::sync::oneshot::channel();
    let root_hook =
        fixture.on_project_scan(root.clone(), server::ScanPoint::ReadDirectory, move || {
            fs::remove_dir_all(&remove_root).expect("remove project root during scan");
            removed_tx
                .send(())
                .expect("root scan observer still waiting");
        });
    fs::write(root.join("trigger-root-scan"), "refresh").expect("trigger root scan");
    (removed_rx).await.expect("root removed");
    // A root that vanishes mid-scan is reported on that root, not by failing
    // the subscription; any CommandError here fails the match.
    let file_list = expect_project_file_list_matching(
        &mut fixture.client,
        "root removed during scan is reported unavailable",
        |list| root_unavailable_message(list, &root).is_some(),
    )
    .await;
    let message = root_unavailable_message(&file_list, &root).unwrap();
    assert!(message.contains(root.to_str().unwrap()));
    assert!(message.contains("No such file or directory"));
    assert!(root_listing(&file_list, &root).entries.is_empty());
    drop(root_hook);
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

    let ProjectBootstrapPayload { file_list, .. } =
        expect_project_bootstrap(&mut fixture.client, "server-pushed project bootstrap").await;
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
}

#[tokio::test]
async fn recent_history_reads_exact_committed_ranges() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo("initial", &[("src/lib.rs", "pub fn value() -> u8 { 1 }\n")]);
    let initial_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);

    write_file(
        &repo.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 2 }\n",
    );
    write_file(
        &repo.path().join("src/added.rs"),
        "pub const ADDED: bool = true;\n",
    );
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "Add committed changes"]);
    let tip_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &["commit", "--allow-empty", "--allow-empty-message", "-m", ""],
    );
    let empty_subject_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    write_file(&repo.path().join("untracked.txt"), "working tree only\n");

    let project = create_project(
        &mut fixture.client,
        "Committed history",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut fixture.client, "history bootstrap").await;
    let root_status = &bootstrap.git_status.roots[0];
    assert_eq!(
        root_status.head_oid.as_deref(),
        Some(empty_subject_oid.as_str())
    );
    assert_eq!(root_status.recent_commits[0].oid, empty_subject_oid);
    assert_eq!(root_status.recent_commits[0].subject, "(no commit message)");
    assert_eq!(root_status.recent_commits[1].oid, tip_oid);
    assert_eq!(
        root_status.recent_commits[1].first_parent_oid.as_deref(),
        Some(initial_oid.as_str())
    );
    assert_eq!(
        root_status.recent_commits[1].subject,
        "Add committed changes"
    );
    assert_eq!(root_status.recent_commits[2].subject, "initial");
    let empty_tree_oid = root_status
        .empty_tree_oid
        .clone()
        .expect("repository-native empty tree oid");

    let root = ProjectRootPath(project_root(&project, 0));
    let revision = protocol::ProjectDiffRevision::CommittedRange {
        base_oid: initial_oid.clone(),
        tip_oid: tip_oid.clone(),
    };
    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            request_id: None,
            root: root.clone(),
            scope: ProjectDiffScope::Uncommitted,
            revision: revision.clone(),
            path: None,
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
        },
        "exact committed range",
    )
    .await;
    assert_eq!(diff.revision, revision);
    let paths = diff
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["src/added.rs", "src/lib.rs"]);
    assert!(!paths.contains(&"untracked.txt"));
    assert_eq!(
        diff.files
            .iter()
            .find(|file| file.relative_path == "src/added.rs")
            .and_then(|file| file.change_kind),
        Some(protocol::ProjectGitChangeKind::Added)
    );
    assert_eq!(
        diff.files
            .iter()
            .find(|file| file.relative_path == "src/lib.rs")
            .and_then(|file| file.change_kind),
        Some(protocol::ProjectGitChangeKind::Modified)
    );

    fixture
        .client
        .project_read_diff(
            &project.id,
            ProjectReadDiffPayload {
                request_id: Some("rewritten-range".to_owned()),
                root: root.clone(),
                scope: ProjectDiffScope::Uncommitted,
                revision: protocol::ProjectDiffRevision::CommittedRange {
                    base_oid: initial_oid.clone(),
                    tip_oid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                },
                path: None,
                context_mode: DiffContextMode::Hunks,
                out_of_band: false,
            },
        )
        .await
        .expect("send rewritten range diff");
    let error = expect_command_error(&mut fixture.client, "rewritten range error").await;
    assert_eq!(error.request_id.as_deref(), Some("rewritten-range"));
    assert_eq!(error.request_kind, FrameKind::ProjectReadDiff);
    assert!(error.message.contains("failed"));

    let root_diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            request_id: None,
            root,
            scope: ProjectDiffScope::Uncommitted,
            revision: protocol::ProjectDiffRevision::CommittedRange {
                base_oid: empty_tree_oid,
                tip_oid: initial_oid,
            },
            path: None,
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
        },
        "root commit range",
    )
    .await;
    assert_eq!(root_diff.files.len(), 1);
    assert_eq!(root_diff.files[0].relative_path, "src/lib.rs");
    assert_eq!(
        root_diff.files[0].change_kind,
        Some(protocol::ProjectGitChangeKind::Added)
    );
}

#[tokio::test]
async fn non_utf8_commit_metadata_keeps_project_bootstrap_usable() {
    let mut fixture = Fixture::new().await;
    let repo = init_git_repo(
        "non-utf8-history",
        &[("src/lib.rs", "pub fn history_survives() {}\n")],
    );
    let parent_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    let tree_oid = git_stdout(repo.path(), &["rev-parse", "HEAD^{tree}"]);
    let mut raw_commit =
        format!("tree {tree_oid}\nparent {parent_oid}\nauthor Invalid ").into_bytes();
    raw_commit.push(0xff);
    raw_commit.extend_from_slice(
        b" Author <author@example.com> 1 +0000\ncommitter Test Committer <committer@example.com> 1 +0000\n\nInvalid ",
    );
    raw_commit.push(0xfe);
    raw_commit.extend_from_slice(b" subject\n");
    let commit_oid = String::from_utf8(git_with_stdin(
        repo.path(),
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        &raw_commit,
    ))
    .expect("commit oid utf-8")
    .trim()
    .to_owned();
    git(repo.path(), &["update-ref", "HEAD", &commit_oid]);

    let project = create_project(
        &mut fixture.client,
        "Non UTF-8 history",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;
    let bootstrap =
        expect_project_bootstrap(&mut fixture.client, "non-utf8 history bootstrap").await;
    assert!(
        bootstrap.file_list.roots[0]
            .entries
            .iter()
            .any(|entry| entry.relative_path == "src/lib.rs"),
        "non-UTF-8 history metadata must not suppress the project file tree"
    );
    let root_status = &bootstrap.git_status.roots[0];
    assert_eq!(root_status.head_oid.as_deref(), Some(commit_oid.as_str()));
    assert!(root_status.clean);
    assert_eq!(root_status.recent_commits[0].oid, commit_oid);
    assert!(root_status.recent_commits[0].author.contains('\u{fffd}'));
    assert!(root_status.recent_commits[0].subject.contains('\u{fffd}'));
    assert!(root_status.empty_tree_oid.is_some());
    assert_eq!(
        project_root(&project, 0),
        repo.path().to_string_lossy().to_string()
    );
}

#[tokio::test]
async fn sha256_root_history_uses_repository_empty_tree() {
    let mut fixture = Fixture::new().await;
    let repo = tempfile::tempdir().expect("sha256 repo");
    git(repo.path(), &["init", "--object-format=sha256"]);
    git(repo.path(), &["config", "user.email", "tests@example.com"]);
    git(repo.path(), &["config", "user.name", "Tests"]);
    write_file(&repo.path().join("root.txt"), "sha256 root\n");
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "SHA-256 root"]);
    let tip_oid = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
    assert_eq!(tip_oid.len(), 64);

    let project = create_project(
        &mut fixture.client,
        "SHA-256 history",
        vec![repo.path().to_string_lossy().to_string()],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut fixture.client, "sha256 bootstrap").await;
    let root_status = &bootstrap.git_status.roots[0];
    let empty_tree_oid = root_status
        .empty_tree_oid
        .clone()
        .expect("sha256 empty tree oid");
    assert_eq!(empty_tree_oid.len(), 64);

    let diff = request_project_diff(
        &mut fixture.client,
        &project.id,
        ProjectReadDiffPayload {
            request_id: Some("sha256-root-diff".to_owned()),
            root: ProjectRootPath(project_root(&project, 0)),
            scope: ProjectDiffScope::Uncommitted,
            revision: protocol::ProjectDiffRevision::CommittedRange {
                base_oid: empty_tree_oid,
                tip_oid,
            },
            path: None,
            context_mode: DiffContextMode::Hunks,
            out_of_band: false,
        },
        "sha256 root diff",
    )
    .await;
    assert_eq!(diff.request_id.as_deref(), Some("sha256-root-diff"));
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].relative_path, "root.txt");
}

async fn restart_moved_agent(
    fixture: &mut Fixture,
    session_id: &protocol::SessionId,
    project_id: Option<&ProjectId>,
    roots: &[String],
) -> (fixture::TestAgent, protocol::AgentStartPayload) {
    // The old flow launched three inspection hosts over the original live
    // stores. The failure logged three backend launches for one explicit
    // spawn: those hosts auto-restored the same session. Startup persistence
    // could then replace the later move's roots with an older resume snapshot.
    // Retire the previous owner before exercising actual auto-restoration.
    let bootstrap = fixture.restart_host().await;
    let saved = bootstrap
        .sessions
        .iter()
        .find(|session| &session.id == session_id)
        .expect("moved session persisted across restart");
    assert_eq!(saved.workspace_roots, roots);
    assert_eq!(saved.project_id.as_ref(), project_id);
    let mut descriptors = bootstrap.agents;
    let env = next_frame_matching_on(&mut fixture.client, "moved agent auto-restoration", |env| {
        if env.kind == FrameKind::NewAgent {
            descriptors.push(env.parse_payload::<protocol::NewAgentPayload>().expect("restored descriptor"));
        }
        env.kind == FrameKind::AgentBootstrap && env.parse_payload::<protocol::AgentBootstrapPayload>()
            .is_ok_and(|payload| payload.events.iter().any(|event| matches!(event,
                protocol::AgentBootstrapEvent::AgentStart(start) if start.session_id.as_ref() == Some(session_id))))
    }).await;
    let replay: protocol::AgentBootstrapPayload =
        env.parse_payload().expect("restored agent bootstrap");
    assert_eq!(
        replay.activity,
        protocol::AgentActivity::Idle,
        "auto-restored moved agent must be idle"
    );
    let start = replay
        .events
        .iter()
        .find_map(|event| match event {
            protocol::AgentBootstrapEvent::AgentStart(start) => Some(start.clone()),
            _ => None,
        })
        .expect("restored start");
    assert!(
        start.session_id.as_ref() == Some(session_id),
        "restart must preserve the conversation identity"
    );
    assert_eq!(start.workspace_roots, roots);
    assert_eq!(start.project_id.as_ref(), project_id);
    let replay_messages = replay
        .events
        .iter()
        .filter_map(|event| match event {
            protocol::AgentBootstrapEvent::ChatEvent(protocol::ChatEvent::MessageAdded(
                message,
            )) => Some(message.content.as_str()),
            protocol::AgentBootstrapEvent::ChatEvent(protocol::ChatEvent::StreamEnd(end)) => {
                Some(end.message.content.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    eprintln!(
        "Move auto-restoration replay: event_count={} message_count={} original_reply_count={}",
        replay.events.len(),
        replay_messages.len(),
        replay_messages
            .iter()
            .filter(|content| **content == "Before moving")
            .count()
    );
    // The authoritative journal replays the actual scripted reply, not the
    // native mock history's regenerated response to the original prompt.
    assert_eq!(
        replay_messages
            .iter()
            .filter(|content| **content == "Before moving")
            .count(),
        1,
        "restart must replay the original conversation exactly once"
    );
    let matching = descriptors
        .iter()
        .filter(|agent| agent.agent_id == start.agent_id)
        .count();
    assert_eq!(
        matching, 1,
        "auto-restoration must publish one descriptor for the owner"
    );
    let descriptor = descriptors
        .into_iter()
        .find(|agent| agent.agent_id == start.agent_id)
        .expect("restored agent descriptor");
    assert_eq!(descriptor.workspace_roots, roots);
    assert_eq!(descriptor.project_id.as_ref(), project_id);
    let (_, settled) = fixture.connect_with_bootstrap().await;
    let saved = settled
        .sessions
        .iter()
        .find(|session| &session.id == session_id)
        .expect("saved session after auto-restoration");
    assert_eq!(saved.workspace_roots, roots);
    assert_eq!(saved.project_id.as_ref(), project_id);
    assert_eq!(
        settled
            .agents
            .iter()
            .filter(|agent| agent.session_id.as_ref() == Some(session_id))
            .count(),
        1,
        "restart must leave exactly one owner of the moved conversation"
    );
    eprintln!(
        "Move restart persisted and auto-restored one owner; root_count={}",
        roots.len()
    );
    (
        fixture::TestAgent {
            stream: descriptor.instance_stream.clone(),
            new_agent: descriptor,
        },
        start,
    )
}

#[tokio::test]
async fn move_agent_preserves_conversation_and_persists_all_project_roots() {
    let mut fixture = Fixture::new().await;
    let origin = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let source = create_project_with_real_roots(
        &mut fixture.client,
        "Source",
        vec![origin.path().to_str().unwrap().to_owned()],
    )
    .await;
    let destination = create_project_with_real_roots(
        &mut fixture.client,
        "Destination",
        vec![
            target.path().to_str().unwrap().to_owned(),
            extra.path().to_str().unwrap().to_owned(),
        ],
    )
    .await;
    let gate = server::backend::mock::MockGateHandle::new();
    let reservation = fixture
        .reserve_next_mock_launch(
            "Moving agent",
            server::backend::mock::MockScript::one(server::backend::mock::MockTurn::gated_text(
                "Before moving",
                &gate,
            )),
        )
        .await;
    let (mut agent, mut start) = fixture
        .spawn_with(protocol::SpawnAgentPayload {
            launch_context: protocol::AgentLaunchContext::Standard,
            name: Some("Moving agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(source.id.clone()),
            params: protocol::SpawnAgentParams::New {
                workspace_roots: project_roots(&source),
                prompt: "Hello before moving".to_owned(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    drop(reservation);
    let session_id = start.session_id.clone().expect("moving session identity");
    gate.wait_until_entered().await;
    fixture
        .client
        .move_agent(protocol::types::AgentMovePayload {
            request_id: "busy".to_owned(),
            agent_id: start.agent_id.clone(),
            project_id: Some(destination.id.clone()),
        })
        .await
        .unwrap();
    let busy = next_frame_matching_on(&mut fixture.client, "busy move rejection", |env| {
        env.kind == FrameKind::AgentMoveResult
    })
    .await
    .parse_payload::<protocol::types::AgentMoveResultPayload>()
    .unwrap();
    assert!(busy.result.unwrap_err().contains("finished"));
    gate.release_one();
    fixture
        .next_chat_event_matching(&agent, "initial turn idle", |event| {
            matches!(event, protocol::ChatEvent::TypingStatusChanged(false))
        })
        .await;
    let mut observer = fixture.connect().await;
    for (request_id, project_id, expected) in [
        (
            "missing",
            Some(ProjectId("missing-project".to_owned())),
            None,
        ),
        (
            "move",
            Some(destination.id.clone()),
            Some(project_roots(&destination)),
        ),
        ("unassign", None, Some(project_roots(&destination))),
        ("unassigned-again", None, Some(project_roots(&destination))),
        (
            "return",
            Some(source.id.clone()),
            Some(project_roots(&source)),
        ),
    ] {
        fixture
            .client
            .move_agent(protocol::types::AgentMovePayload {
                request_id: request_id.to_owned(),
                agent_id: start.agent_id.clone(),
                project_id: project_id.clone(),
            })
            .await
            .unwrap();
        let result = next_frame_matching_on(&mut fixture.client, "move result", |env| {
            env.kind == FrameKind::AgentMoveResult
        })
        .await
        .parse_payload::<protocol::types::AgentMoveResultPayload>()
        .unwrap();
        assert_eq!(result.request_id, request_id);
        match expected {
            None => assert!(result.result.is_err()),
            Some(roots) => {
                let moved = result.result.unwrap();
                assert_eq!(moved.agent_id, start.agent_id);
                assert_eq!(moved.session_id, start.session_id);
                assert_eq!(moved.project_id, project_id);
                assert_eq!(moved.workspace_roots, roots);
                let broadcast = next_frame_matching_on(&mut observer, "observer move", |env| {
                    env.kind == FrameKind::AgentMoveResult
                })
                .await
                .parse_payload::<protocol::types::AgentMoveResultPayload>()
                .unwrap();
                assert_eq!(broadcast.result.unwrap().project_id, project_id.clone());
                eprintln!(
                    "PROJECT MOVE PERSISTENCE connecting observer expected_root_count={}",
                    roots.len()
                );
                // A fresh host restores open agents and becomes another writer.
                // Keep one owner while moving; bootstrap reads persisted SQLite
                // rows, and the real host restart below checks recovery afterward.
                let (_, persisted) = fixture.connect_with_bootstrap().await;
                let saved = persisted
                    .sessions
                    .iter()
                    .find(|session| Some(&session.id) == start.session_id.as_ref())
                    .expect("moved session persisted");
                eprintln!(
                    "PROJECT MOVE PERSISTENCE bootstrap root_count={} matches_expected={} project_matches_expected={}",
                    saved.workspace_roots.len(),
                    saved.workspace_roots == roots,
                    saved.project_id == moved.project_id
                );
                assert_eq!(saved.workspace_roots, roots);
                assert_eq!(saved.project_id, moved.project_id);
                let continued = format!("Continued after {request_id}");
                fixture
                    .mock(&agent)
                    .await
                    .enqueue(server::backend::mock::MockTurn::text(continued.clone()))
                    .await;
                fixture
                    .client
                    .send_message(&agent.stream, "Continue the same chat".to_owned())
                    .await
                    .unwrap();
                let turn = fixture.finish_turn(&agent).await;
                assert!(
                    turn.chat_events().iter().any(|event| matches!(event,
                    protocol::ChatEvent::StreamEnd(end) if end.message.content == continued)),
                    "the live conversation must continue after moving"
                );
                let prior_agent_id = start.agent_id.clone();
                (agent, start) =
                    restart_moved_agent(&mut fixture, &session_id, project_id.as_ref(), &roots)
                        .await;
                assert!(
                    start.agent_id == prior_agent_id,
                    "restart must reconstruct the owner under its persisted agent id"
                );
                observer = fixture.connect().await;
            }
        }
    }
    let (restored, restored_start) = restart_moved_agent(
        &mut fixture,
        &session_id,
        Some(&source.id),
        &project_roots(&source),
    )
    .await;
    assert!(
        restored_start.session_id == start.session_id,
        "final restart must preserve the session"
    );
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    let session = bootstrap
        .sessions
        .iter()
        .find(|session| Some(&session.id) == start.session_id.as_ref())
        .expect("saved session after host restart");
    eprintln!(
        "PROJECT MOVE PERSISTENCE final bootstrap root_count={} matches_source={} project_matches_source={}",
        session.workspace_roots.len(),
        session.workspace_roots == project_roots(&source),
        session.project_id == Some(source.id.clone())
    );
    assert_eq!(session.project_id, Some(source.id.clone()));
    assert_eq!(session.workspace_roots, project_roots(&source));
    fixture
        .client
        .send_message(&restored.stream, "Continue after final restart".to_owned())
        .await
        .expect("send to auto-restored conversation");
    let turn = fixture.finish_turn(&restored).await;
    assert!(turn.chat_events().iter().any(|event| matches!(event,
        protocol::ChatEvent::StreamEnd(end) if end.message.content.contains("Continue after final restart"))),
        "the persisted conversation must remain usable after final restart");
}
