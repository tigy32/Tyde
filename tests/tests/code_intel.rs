mod fixture;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    CodeIntelErrorCode, CodeIntelErrorContext, CodeIntelErrorPayload, CodeIntelState,
    CodeIntelStatusPayload, CodeIntelSubscribeFilePayload, Envelope, FrameKind, Project,
    ProjectCreatePayload, ProjectId, ProjectNotifyPayload, ProjectPath, ProjectRootPath,
    StreamPath, read_envelope, write_envelope,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
        if matches!(
            env.kind,
            FrameKind::HostSettings
                | FrameKind::SessionSchemas
                | FrameKind::BackendSetup
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionList
                | FrameKind::ProjectEvent
        ) {
            continue;
        }
        return env;
    }
}

async fn expect_project_notify(
    client: &mut client::Connection,
    context: &str,
) -> ProjectNotifyPayload {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::ProjectNotify {
            return env
                .parse_payload()
                .expect("failed to parse ProjectNotifyPayload");
        }
        if matches!(
            env.kind,
            FrameKind::ProjectFileList | FrameKind::ProjectGitStatus
        ) {
            continue;
        }
        assert_eq!(env.kind, FrameKind::ProjectNotify);
    }
}

async fn expect_project_bootstrap(client: &mut client::Connection, context: &str) {
    loop {
        let env = expect_next_event(client, context).await;
        if env.kind == FrameKind::ProjectBootstrap {
            return;
        }
        if matches!(
            env.kind,
            FrameKind::ProjectFileList | FrameKind::ProjectGitStatus
        ) {
            continue;
        }
        assert_eq!(env.kind, FrameKind::ProjectBootstrap);
    }
}

async fn drain_initial_project_state_pushes(client: &mut client::Connection, context: &str) {
    loop {
        match tokio::time::timeout(Duration::from_millis(100), client.next_event()).await {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => panic!("next_event failed while draining {context}: {err:?}"),
            Ok(Ok(Some(env)))
                if fixture::is_builtin_team_custom_agent_notify(&env)
                    || matches!(
                        env.kind,
                        FrameKind::HostSettings
                            | FrameKind::SessionSchemas
                            | FrameKind::BackendSetup
                            | FrameKind::QueuedMessages
                            | FrameKind::SessionSettings
                            | FrameKind::TeamPresetCatalogNotify
                            | FrameKind::SessionList
                            | FrameKind::ProjectEvent
                            | FrameKind::ProjectBootstrap
                            | FrameKind::ProjectFileList
                            | FrameKind::ProjectGitStatus
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

async fn create_project_with_root(client: &mut client::Connection, root: &Path) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: "Code Intel".to_owned(),
            roots: vec![ProjectRootPath(root.to_string_lossy().into_owned())],
        })
        .await
        .expect("project_create failed");

    let project = match expect_project_notify(client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    };
    expect_project_bootstrap(client, "initial project bootstrap").await;
    drain_initial_project_state_pushes(client, "initial project state pushes").await;
    project
}

async fn send_code_intel_subscribe(
    client: &mut client::Connection,
    project_id: &ProjectId,
    path: ProjectPath,
) {
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let seq = client
        .outgoing_seq
        .get(&stream)
        .copied()
        .expect("missing project stream sequence counter");
    let payload = CodeIntelSubscribeFilePayload { path };
    let envelope = Envelope::from_payload(
        stream.clone(),
        FrameKind::CodeIntelSubscribeFile,
        seq,
        &payload,
    )
    .expect("serialize code_intel_subscribe_file");
    client.outgoing_seq.insert(stream, seq + 1);
    write_envelope(&mut client.writer, &envelope)
        .await
        .expect("write code_intel_subscribe_file");
}

async fn next_raw_event(client: &mut client::Connection, context: &str) -> Envelope {
    let env = match tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut client.reader))
        .await
    {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("read_envelope failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    };
    client
        .incoming_seq
        .validate(&env.stream, env.seq, env.kind)
        .expect("incoming sequence must be valid");
    env
}

async fn wait_for_code_intel_unavailable(
    client: &mut client::Connection,
) -> (CodeIntelStatusPayload, CodeIntelErrorPayload) {
    let mut unavailable = None;
    let mut error = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while unavailable.is_none() || error.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for code-intel unavailable/error frames"
        );
        let env = next_raw_event(client, "code-intel error").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::CodeIntelStatus => {
                let payload: CodeIntelStatusPayload =
                    env.parse_payload().expect("parse CodeIntelStatusPayload");
                if payload.state == CodeIntelState::Unavailable {
                    unavailable = Some(payload);
                }
            }
            FrameKind::CodeIntelError => {
                error = Some(env.parse_payload().expect("parse CodeIntelErrorPayload"));
            }
            FrameKind::ProjectFileList
            | FrameKind::ProjectGitStatus
            | FrameKind::ProjectEvent
            | FrameKind::HostSettings
            | FrameKind::SessionSchemas
            | FrameKind::BackendSetup
            | FrameKind::QueuedMessages
            | FrameKind::SessionSettings
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList => {}
            other => panic!("unexpected frame while waiting for code-intel error: {other}"),
        }
    }
    (unavailable.unwrap(), error.unwrap())
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut perms = fs::metadata(path).expect("stat executable").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod executable");
}

#[cfg(unix)]
struct PathGuard {
    old_path: Option<OsString>,
}

#[cfg(unix)]
impl PathGuard {
    fn prepend(path: PathBuf) -> Self {
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![path];
        if let Some(old_path) = old_path.as_ref() {
            paths.extend(std::env::split_paths(old_path));
        }
        let joined = std::env::join_paths(paths).expect("join PATH entries");
        unsafe {
            std::env::set_var("PATH", joined);
        }
        Self { old_path }
    }
}

#[cfg(unix)]
impl Drop for PathGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.old_path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn rust_analyzer_broken_path_hit_surfaces_actionable_error() {
    let bin_dir = tempfile::tempdir().expect("create fake bin dir");
    write_executable(
        &bin_dir.path().join("rust-analyzer"),
        "#!/bin/sh\necho \"error: Unknown binary 'rust-analyzer' in official toolchain\" >&2\nexit 1\n",
    );
    write_executable(
        &bin_dir.path().join("rustup"),
        "#!/bin/sh\necho \"rustup: component 'rust-analyzer' is not installed\" >&2\nexit 1\n",
    );

    let _path_guard = PathGuard::prepend(bin_dir.path().to_path_buf());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
        .block_on(async {
            let workspace = tempfile::tempdir().expect("create workspace");
            fs::write(
                workspace.path().join("Cargo.toml"),
                "[package]\nname = \"ci_broken_ra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .expect("write Cargo.toml");
            fs::create_dir_all(workspace.path().join("src")).expect("create src");
            fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n")
                .expect("write main.rs");

            let mut fixture = Fixture::new().await;
            let project = create_project_with_root(&mut fixture.client, workspace.path()).await;
            let path = ProjectPath {
                root: ProjectRootPath(workspace.path().to_string_lossy().into_owned()),
                relative_path: "src/main.rs".to_owned(),
            };

            send_code_intel_subscribe(&mut fixture.client, &project.id, path).await;
            let (status, error) = wait_for_code_intel_unavailable(&mut fixture.client).await;

            assert_eq!(status.state, CodeIntelState::Unavailable);
            assert!(
                status
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("rustup component add rust-analyzer")),
                "Unavailable status should carry the install hint, got {status:?}"
            );
            assert_eq!(error.code, CodeIntelErrorCode::ProviderUnavailable);
            assert!(error.fatal);
            assert!(matches!(
                error.context,
                CodeIntelErrorContext::Provider { .. }
            ));
            assert!(
                error.message.contains("rustup component add rust-analyzer"),
                "error message should include the install hint, got {error:?}"
            );
            assert_eq!(
                error.hint.as_deref(),
                Some("rustup component add rust-analyzer")
            );
            assert!(
                error
                    .stderr
                    .as_deref()
                    .is_some_and(|stderr| stderr.contains("Unknown binary 'rust-analyzer'")),
                "error payload should carry probe stderr, got {error:?}"
            );
            assert!(
                error.exit_status.is_some(),
                "error payload should carry the failed probe exit status, got {error:?}"
            );
        });
}
