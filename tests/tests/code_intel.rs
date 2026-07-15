mod fixture;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fixture::Fixture;
use protocol::BackendKind;
use protocol::{
    CodeIntelErrorCode, CodeIntelErrorContext, CodeIntelErrorPayload, CodeIntelLanguageId,
    CodeIntelOverviewHeadline, CodeIntelOverviewPayload, CodeIntelProviderId,
    CodeIntelProviderStatus, CodeIntelState, CodeIntelStatusPayload, CodeIntelStatusScope,
    CodeIntelSubscribeFilePayload, Envelope, FrameKind, HostExecutablePath, HostSettingValue,
    HostSettingsPayload, Project, ProjectCreatePayload, ProjectFileVersion, ProjectId,
    ProjectNotifyPayload, ProjectPath, ProjectRootPath, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload, StreamPath, read_envelope, write_envelope,
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
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::BackendSetup
                | FrameKind::BackendCapacity
                | FrameKind::QueuedMessages
                | FrameKind::SessionSettings
                | FrameKind::TeamPresetCatalogNotify
                | FrameKind::SessionList
                | FrameKind::TaskTokenUsage
                | FrameKind::WorkflowNotify
                | FrameKind::AgentsViewPreferencesNotify
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
            FrameKind::ProjectFileList
                | FrameKind::ProjectGitStatus
                | FrameKind::CodeIntelOverview
                | FrameKind::TaskTokenUsage
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
            FrameKind::ProjectFileList
                | FrameKind::ProjectGitStatus
                | FrameKind::CodeIntelOverview
                | FrameKind::TaskTokenUsage
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
                            | FrameKind::LaunchProfileCatalogNotify
                            | FrameKind::BackendSetup
                            | FrameKind::BackendCapacity
                            | FrameKind::QueuedMessages
                            | FrameKind::SessionSettings
                            | FrameKind::TeamPresetCatalogNotify
                            | FrameKind::SessionList
                            | FrameKind::WorkflowNotify
                            | FrameKind::AgentsViewPreferencesNotify
                            | FrameKind::ProjectEvent
                            | FrameKind::ProjectBootstrap
                            | FrameKind::ProjectFileList
                            | FrameKind::ProjectGitStatus
                            | FrameKind::CodeIntelOverview
                            | FrameKind::TaskTokenUsage
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

async fn set_rust_analyzer_path(client: &mut client::Connection, path: Option<HostExecutablePath>) {
    send_rust_analyzer_path_setting(client, path.clone()).await;
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());

    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before rust-analyzer path HostSettings"),
            Ok(Err(err)) => panic!("next_event failed before HostSettings: {err:?}"),
            Err(_) => panic!("timed out waiting for rust-analyzer path HostSettings"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        if env.kind != FrameKind::HostSettings {
            continue;
        }
        let payload: HostSettingsPayload = env.parse_payload().expect("parse HostSettings");
        assert_eq!(
            payload
                .settings
                .code_intel
                .language_server_paths
                .get(&provider)
                .cloned(),
            path
        );
        return;
    }
}

async fn send_rust_analyzer_path_setting(
    client: &mut client::Connection,
    path: Option<HostExecutablePath>,
) {
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::CodeIntelLanguageServerPath { provider, path },
        })
        .await
        .expect("set rust-analyzer path setting");
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
            | FrameKind::CodeIntelOverview
            | FrameKind::ProjectEvent
            | FrameKind::HostSettings
            | FrameKind::NewAgent
            | FrameKind::AgentBootstrap
            | FrameKind::AgentStart
            | FrameKind::AgentError
            | FrameKind::AgentActivitySummary
            | FrameKind::AgentActivityStats
            | FrameKind::TaskTokenUsage
            | FrameKind::ChatEvent
            | FrameKind::SessionSchemas
            | FrameKind::LaunchProfileCatalogNotify
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

async fn wait_for_code_intel_unavailable_with_overview(
    client: &mut client::Connection,
    root: &ProjectRootPath,
    provider: &CodeIntelProviderId,
) -> (
    CodeIntelStatusPayload,
    CodeIntelErrorPayload,
    CodeIntelOverviewPayload,
) {
    let mut unavailable = None;
    let mut error = None;
    let mut overview = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while unavailable.is_none() || error.is_none() || overview.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for code-intel unavailable/error/overview frames"
        );
        let env = next_raw_event(client, "code-intel unavailable overview").await;
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
            FrameKind::CodeIntelOverview => {
                let payload: CodeIntelOverviewPayload =
                    env.parse_payload().expect("parse CodeIntelOverviewPayload");
                let provider_unavailable = payload.summary.headline
                    == CodeIntelOverviewHeadline::Unavailable
                    && payload.roots.iter().any(|root_overview| {
                        &root_overview.root == root
                            && root_overview.providers.iter().any(|provider_status| {
                                &provider_status.provider == provider
                                    && provider_status.state == CodeIntelState::Unavailable
                            })
                    });
                if provider_unavailable {
                    overview = Some(payload);
                }
            }
            FrameKind::ProjectFileList
            | FrameKind::ProjectGitStatus
            | FrameKind::ProjectEvent
            | FrameKind::HostSettings
            | FrameKind::NewAgent
            | FrameKind::AgentBootstrap
            | FrameKind::AgentStart
            | FrameKind::AgentError
            | FrameKind::AgentActivitySummary
            | FrameKind::AgentActivityStats
            | FrameKind::TaskTokenUsage
            | FrameKind::ChatEvent
            | FrameKind::SessionSchemas
            | FrameKind::LaunchProfileCatalogNotify
            | FrameKind::BackendSetup
            | FrameKind::QueuedMessages
            | FrameKind::SessionSettings
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList
            | FrameKind::WorkflowNotify
            | FrameKind::AgentsViewPreferencesNotify => {}
            other => panic!("unexpected frame while waiting for code-intel overview: {other}"),
        }
    }
    (unavailable.unwrap(), error.unwrap(), overview.unwrap())
}

fn overview_provider_status<'a>(
    overview: &'a CodeIntelOverviewPayload,
    root: &ProjectRootPath,
    provider: &CodeIntelProviderId,
) -> Option<&'a CodeIntelProviderStatus> {
    overview
        .roots
        .iter()
        .find(|root_overview| &root_overview.root == root)
        .and_then(|root_overview| {
            root_overview
                .providers
                .iter()
                .find(|provider_status| &provider_status.provider == provider)
        })
}

async fn wait_for_code_intel_warm_unavailable_overview(
    client: &mut client::Connection,
    root: &ProjectRootPath,
    provider: &CodeIntelProviderId,
) -> CodeIntelOverviewPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for code-intel warm unavailable overview"
        );
        let env = next_raw_event(client, "code-intel warm overview").await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::CodeIntelOverview => {
                let payload: CodeIntelOverviewPayload =
                    env.parse_payload().expect("parse CodeIntelOverviewPayload");
                let provider_unavailable = payload.summary.headline
                    == CodeIntelOverviewHeadline::Unavailable
                    && overview_provider_status(&payload, root, provider).is_some_and(
                        |provider_status| provider_status.state == CodeIntelState::Unavailable,
                    );
                if provider_unavailable {
                    return payload;
                }
            }
            FrameKind::CodeIntelStatus | FrameKind::CodeIntelError => {
                panic!(
                    "warm without file subscription must surface through CodeIntelOverview only, got {} on {}",
                    env.kind, env.stream
                );
            }
            FrameKind::ProjectBootstrap
            | FrameKind::ProjectFileList
            | FrameKind::ProjectGitStatus
            | FrameKind::ProjectEvent
            | FrameKind::HostSettings
            | FrameKind::NewAgent
            | FrameKind::AgentBootstrap
            | FrameKind::AgentStart
            | FrameKind::AgentError
            | FrameKind::AgentActivitySummary
            | FrameKind::AgentActivityStats
            | FrameKind::TaskTokenUsage
            | FrameKind::ChatEvent
            | FrameKind::SessionSchemas
            | FrameKind::LaunchProfileCatalogNotify
            | FrameKind::BackendSetup
            | FrameKind::QueuedMessages
            | FrameKind::SessionSettings
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList
            | FrameKind::WorkflowNotify
            | FrameKind::AgentsViewPreferencesNotify => {}
            other => panic!("unexpected frame while waiting for warm overview: {other}"),
        }
    }
}

async fn wait_for_code_intel_status_matching(
    client: &mut client::Connection,
    context: &str,
    mut predicate: impl FnMut(&CodeIntelStatusPayload) -> bool,
) -> CodeIntelStatusPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for code-intel status matching {context}"
        );
        let env = next_raw_event(client, context).await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::CodeIntelStatus => {
                let payload: CodeIntelStatusPayload =
                    env.parse_payload().expect("parse CodeIntelStatusPayload");
                if predicate(&payload) {
                    return payload;
                }
            }
            FrameKind::ProjectFileList
            | FrameKind::ProjectGitStatus
            | FrameKind::CodeIntelOverview
            | FrameKind::ProjectEvent
            | FrameKind::HostSettings
            | FrameKind::SessionSchemas
            | FrameKind::LaunchProfileCatalogNotify
            | FrameKind::BackendSetup
            | FrameKind::QueuedMessages
            | FrameKind::SessionSettings
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList
            | FrameKind::TaskTokenUsage
            | FrameKind::WorkflowNotify
            | FrameKind::AgentsViewPreferencesNotify
            | FrameKind::CodeIntelError => {}
            other => panic!("unexpected frame while waiting for code-intel status: {other}"),
        }
    }
}

async fn wait_for_code_intel_overview_matching(
    client: &mut client::Connection,
    context: &str,
    mut predicate: impl FnMut(&CodeIntelOverviewPayload) -> bool,
) -> CodeIntelOverviewPayload {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for code-intel overview matching {context}"
        );
        let env = next_raw_event(client, context).await;
        if fixture::is_builtin_team_custom_agent_notify(&env) {
            continue;
        }
        match env.kind {
            FrameKind::CodeIntelOverview => {
                let payload: CodeIntelOverviewPayload =
                    env.parse_payload().expect("parse CodeIntelOverviewPayload");
                if predicate(&payload) {
                    return payload;
                }
            }
            FrameKind::ProjectFileList
            | FrameKind::ProjectGitStatus
            | FrameKind::ProjectEvent
            | FrameKind::HostSettings
            | FrameKind::SessionSchemas
            | FrameKind::LaunchProfileCatalogNotify
            | FrameKind::BackendSetup
            | FrameKind::QueuedMessages
            | FrameKind::SessionSettings
            | FrameKind::TeamPresetCatalogNotify
            | FrameKind::SessionList
            | FrameKind::TaskTokenUsage
            | FrameKind::WorkflowNotify
            | FrameKind::AgentsViewPreferencesNotify
            | FrameKind::CodeIntelStatus
            | FrameKind::CodeIntelError => {}
            other => panic!("unexpected frame while waiting for code-intel overview: {other}"),
        }
    }
}

async fn assert_no_code_intel_warm_events(client: &mut client::Connection, context: &str) {
    loop {
        match tokio::time::timeout(
            Duration::from_millis(250),
            read_envelope(&mut client.reader),
        )
        .await
        {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => panic!("read_envelope failed while checking {context}: {err:?}"),
            Ok(Ok(Some(env))) => {
                client
                    .incoming_seq
                    .validate(&env.stream, env.seq, env.kind)
                    .expect("incoming sequence must be valid");
                if fixture::is_builtin_team_custom_agent_notify(&env) {
                    continue;
                }
                match env.kind {
                    FrameKind::CodeIntelOverview
                    | FrameKind::CodeIntelStatus
                    | FrameKind::CodeIntelError => {
                        panic!(
                            "unexpected code-intel warm event while checking {context}: kind={} stream={}",
                            env.kind, env.stream
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn assert_no_direct_code_intel_warm_frames(client: &mut client::Connection, context: &str) {
    loop {
        match tokio::time::timeout(
            Duration::from_millis(250),
            read_envelope(&mut client.reader),
        )
        .await
        {
            Err(_) => return,
            Ok(Ok(None)) => return,
            Ok(Err(err)) => panic!("read_envelope failed while checking {context}: {err:?}"),
            Ok(Ok(Some(env))) => {
                client
                    .incoming_seq
                    .validate(&env.stream, env.seq, env.kind)
                    .expect("incoming sequence must be valid");
                if fixture::is_builtin_team_custom_agent_notify(&env) {
                    continue;
                }
                match env.kind {
                    FrameKind::CodeIntelStatus | FrameKind::CodeIntelError => {
                        panic!(
                            "unexpected direct code-intel warm frame while checking {context}: kind={} stream={}",
                            env.kind, env.stream
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn project_subscribe_emits_idle_code_intel_overview_for_all_roots() {
    let mut fixture = Fixture::new().await;
    let root_a = tempfile::tempdir().expect("create root a");
    let root_b = tempfile::tempdir().expect("create root b");
    let root_a_path = ProjectRootPath(root_a.path().to_string_lossy().into_owned());
    let root_b_path = ProjectRootPath(root_b.path().to_string_lossy().into_owned());

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Code Intel Multi Root".to_owned(),
            roots: vec![root_a_path.clone(), root_b_path.clone()],
        })
        .await
        .expect("project_create failed");

    let project = match expect_project_notify(&mut fixture.client, "project create").await {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    };
    expect_project_bootstrap(&mut fixture.client, "initial project bootstrap").await;
    let overview = wait_for_code_intel_overview_matching(
        &mut fixture.client,
        "initial idle overview",
        |overview| {
            overview.roots.len() == 2 && overview.roots.iter().all(|root| root.providers.is_empty())
        },
    )
    .await;

    assert_eq!(project.root_paths(), vec![root_a_path, root_b_path]);
    assert_eq!(overview.roots.len(), 2);
    assert_eq!(overview.roots[0].root, project.root_paths()[0]);
    assert_eq!(overview.roots[1].root, project.root_paths()[1]);
    assert_eq!(
        overview.summary.headline,
        CodeIntelOverviewHeadline::NotStarted
    );
    assert_eq!(overview.summary.ready, 0);
    assert_eq!(overview.summary.indexing, 0);
    assert_eq!(overview.summary.starting, 0);
    assert_eq!(
        overview.summary.message.as_deref(),
        Some("No language server running — select the project or launch an agent to index")
    );
}

#[tokio::test]
async fn project_bootstrap_with_markers_does_not_start_code_intel() {
    let mut fixture = Fixture::new().await;
    let missing_path = tempfile::tempdir()
        .expect("create configured path tempdir")
        .path()
        .join("missing-rust-analyzer");
    set_rust_analyzer_path(
        &mut fixture.client,
        Some(HostExecutablePath(
            missing_path.to_string_lossy().into_owned(),
        )),
    )
    .await;

    let root = tempfile::tempdir().expect("create root");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"ci_lazy_bootstrap\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    let root_path = ProjectRootPath(root.path().to_string_lossy().into_owned());

    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Code Intel Lazy Bootstrap".to_owned(),
            roots: vec![root_path.clone()],
        })
        .await
        .expect("project_create failed");
    let _ = expect_project_notify(&mut fixture.client, "project create").await;
    expect_project_bootstrap(&mut fixture.client, "initial project bootstrap").await;
    let overview = wait_for_code_intel_overview_matching(
        &mut fixture.client,
        "bootstrap-only overview",
        |overview| {
            overview.summary.headline == CodeIntelOverviewHeadline::NotStarted
                && overview
                    .roots
                    .iter()
                    .any(|root| root.root == root_path && root.providers.is_empty())
        },
    )
    .await;
    assert_eq!(overview.summary.ready, 0);
    assert_eq!(overview.summary.starting, 0);
    assert_eq!(overview.summary.unavailable, 0);
    assert_no_code_intel_warm_events(&mut fixture.client, "bootstrap-only project").await;
}

#[tokio::test]
async fn project_accessed_warms_code_intel_without_file_subscribe_and_is_idempotent() {
    let mut fixture = Fixture::new().await;
    let missing_path = tempfile::tempdir()
        .expect("create configured path tempdir")
        .path()
        .join("missing-rust-analyzer");
    let missing_path_text = missing_path.to_string_lossy().into_owned();
    set_rust_analyzer_path(
        &mut fixture.client,
        Some(HostExecutablePath(missing_path_text.clone())),
    )
    .await;

    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"ci_project_accessed\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::create_dir_all(workspace.path().join("src")).expect("create src");
    fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
    let project = create_project_with_root(&mut fixture.client, workspace.path()).await;
    let root = ProjectRootPath(workspace.path().to_string_lossy().into_owned());
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());
    let file_path = ProjectPath {
        root: root.clone(),
        relative_path: "src/main.rs".to_owned(),
    };

    fixture
        .client
        .project_accessed(&project.id)
        .await
        .expect("project_accessed failed");
    let overview =
        wait_for_code_intel_warm_unavailable_overview(&mut fixture.client, &root, &provider).await;
    let overview_provider = overview_provider_status(&overview, &root, &provider)
        .expect("warm provider status in overview");
    assert_eq!(
        overview_provider.language,
        CodeIntelLanguageId("rust".to_owned())
    );
    assert_eq!(overview_provider.state, CodeIntelState::Unavailable);
    let overview_message = overview_provider
        .message
        .as_deref()
        .expect("warm provider overview message");
    assert!(
        overview_message.contains(&missing_path_text),
        "warm overview should name configured path, got {overview_provider:?}"
    );
    assert_eq!(
        overview.summary.headline,
        CodeIntelOverviewHeadline::Unavailable
    );
    assert_no_direct_code_intel_warm_frames(&mut fixture.client, "first project access").await;

    fixture
        .client
        .project_accessed(&project.id)
        .await
        .expect("second project_accessed failed");
    assert_no_code_intel_warm_events(&mut fixture.client, "second project access").await;

    send_code_intel_subscribe(&mut fixture.client, &project.id, file_path.clone()).await;
    let status = wait_for_code_intel_status_matching(
        &mut fixture.client,
        "post-warm file subscribe",
        |status| {
            status.state == CodeIntelState::Unavailable
                && matches!(
                    &status.scope,
                    CodeIntelStatusScope::File { path, version }
                        if path == &file_path && *version == ProjectFileVersion(0)
                )
        },
    )
    .await;
    assert!(
        status
            .message
            .as_deref()
            .is_some_and(|message| message.contains(&missing_path_text)),
        "post-warm file status should carry unavailable provider message, got {status:?}"
    );
}

#[tokio::test]
async fn agent_launch_with_project_id_warms_code_intel() {
    let mut fixture = Fixture::new().await;
    let missing_path = tempfile::tempdir()
        .expect("create configured path tempdir")
        .path()
        .join("missing-rust-analyzer");
    let missing_path_text = missing_path.to_string_lossy().into_owned();
    set_rust_analyzer_path(
        &mut fixture.client,
        Some(HostExecutablePath(missing_path_text.clone())),
    )
    .await;

    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"ci_agent_launch\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    let project = create_project_with_root(&mut fixture.client, workspace.path()).await;
    let root = ProjectRootPath(workspace.path().to_string_lossy().into_owned());
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());

    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("code-intel-warm".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: vec![workspace.path().to_string_lossy().into_owned()],
                prompt: "warm code intelligence".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent failed");

    let overview =
        wait_for_code_intel_warm_unavailable_overview(&mut fixture.client, &root, &provider).await;
    let overview_provider = overview_provider_status(&overview, &root, &provider)
        .expect("agent-launch warm provider status in overview");
    assert_eq!(
        overview_provider.language,
        CodeIntelLanguageId("rust".to_owned())
    );
    assert_eq!(overview_provider.state, CodeIntelState::Unavailable);
    let overview_message = overview_provider
        .message
        .as_deref()
        .expect("agent-launch warm provider overview message");
    assert!(
        overview_message.contains(&missing_path_text),
        "agent-launch warm overview should name configured path, got {overview_provider:?}"
    );
    assert_eq!(
        overview.summary.headline,
        CodeIntelOverviewHeadline::Unavailable
    );
    assert_no_direct_code_intel_warm_frames(&mut fixture.client, "agent-launch warm").await;
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

#[tokio::test]
async fn configured_invalid_rust_analyzer_path_fails_without_fallback() {
    let configured_path = tempfile::tempdir()
        .expect("create configured path tempdir")
        .path()
        .join("missing-rust-analyzer");
    let configured_path_text = configured_path.to_string_lossy().into_owned();

    let mut fixture = Fixture::new().await;
    set_rust_analyzer_path(
        &mut fixture.client,
        Some(HostExecutablePath(configured_path_text.clone())),
    )
    .await;

    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"ci_configured_missing_ra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::create_dir_all(workspace.path().join("src")).expect("create src");
    fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n").expect("write main.rs");

    let project = create_project_with_root(&mut fixture.client, workspace.path()).await;
    let root = ProjectRootPath(workspace.path().to_string_lossy().into_owned());
    let path = ProjectPath {
        root: root.clone(),
        relative_path: "src/main.rs".to_owned(),
    };

    send_code_intel_subscribe(&mut fixture.client, &project.id, path).await;
    let provider = CodeIntelProviderId("rust-analyzer".to_owned());
    let (status, error, overview) =
        wait_for_code_intel_unavailable_with_overview(&mut fixture.client, &root, &provider).await;

    assert_eq!(status.state, CodeIntelState::Unavailable);
    assert_eq!(
        overview.summary.headline,
        CodeIntelOverviewHeadline::Unavailable
    );
    assert_eq!(overview.summary.unavailable, 1);
    let overview_provider = overview
        .roots
        .iter()
        .find(|root_overview| root_overview.root == root)
        .and_then(|root_overview| {
            root_overview
                .providers
                .iter()
                .find(|provider_status| provider_status.provider == provider)
        })
        .expect("unavailable provider status in overview");
    assert_eq!(overview_provider.state, CodeIntelState::Unavailable);
    let status_message = status
        .message
        .as_deref()
        .expect("configured path status message");
    assert!(
        status_message.contains(&configured_path_text),
        "status should name configured path, got {status:?}"
    );
    assert!(
        !status_message.contains("rustup component add rust-analyzer"),
        "configured path status must not suggest rustup component add, got {status:?}"
    );
    assert_eq!(error.code, CodeIntelErrorCode::ProviderUnavailable);
    assert!(error.fatal);
    assert!(
        error.message.contains(&configured_path_text),
        "error should name configured path, got {error:?}"
    );
    assert!(
        !error.message.contains("rustup component add rust-analyzer"),
        "configured path error must not suggest rustup component add, got {error:?}"
    );
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| !hint.contains("rustup component add rust-analyzer")),
        "configured path hint must not be the rustup component hint, got {error:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn updating_configured_rust_analyzer_path_rediscovers_existing_provider() {
    let mut fixture = Fixture::new().await;

    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"ci_hot_reload_ra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::create_dir_all(workspace.path().join("src")).expect("create src");
    fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n").expect("write main.rs");

    let missing_path = workspace.path().join("missing-rust-analyzer");
    let missing_path_text = missing_path.to_string_lossy().into_owned();
    set_rust_analyzer_path(
        &mut fixture.client,
        Some(HostExecutablePath(missing_path_text.clone())),
    )
    .await;

    let project = create_project_with_root(&mut fixture.client, workspace.path()).await;
    let path = ProjectPath {
        root: ProjectRootPath(workspace.path().to_string_lossy().into_owned()),
        relative_path: "src/main.rs".to_owned(),
    };

    send_code_intel_subscribe(&mut fixture.client, &project.id, path).await;
    let (status, error) = wait_for_code_intel_unavailable(&mut fixture.client).await;
    assert!(
        status
            .message
            .as_deref()
            .is_some_and(|message| message.contains(&missing_path_text)),
        "initial unavailable status should name the bad configured path, got {status:?}"
    );
    assert!(
        error.message.contains(&missing_path_text),
        "initial unavailable error should name the bad configured path, got {error:?}"
    );

    let valid_path = workspace.path().join("fake-rust-analyzer");
    write_executable(
        &valid_path,
        r#"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) > 1 and sys.argv[1] == "--version":
    print("rust-analyzer fake")
    sys.exit(0)

def send(payload):
    data = json.dumps(payload, separators=(",", ":")).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(data)).encode() + b"\r\n\r\n" + data)
    sys.stdout.buffer.flush()

while True:
    headers = {}
    line = sys.stdin.buffer.readline()
    if not line:
        break
    while line not in (b"\r\n", b"\n", b""):
        key, _, value = line.decode("ascii", "replace").partition(":")
        headers[key.lower()] = value.strip()
        line = sys.stdin.buffer.readline()
    length = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(length)
    if not body:
        break
    message = json.loads(body)
    method = message.get("method")
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "semanticTokensProvider": {
                        "legend": {"tokenTypes": ["variable"], "tokenModifiers": []},
                        "full": True
                    }
                }
            }
        })
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        break
"#,
    );

    send_rust_analyzer_path_setting(
        &mut fixture.client,
        Some(HostExecutablePath(
            valid_path.to_string_lossy().into_owned(),
        )),
    )
    .await;

    let status = wait_for_code_intel_status_matching(
        &mut fixture.client,
        "hot-reloaded rust-analyzer path",
        |status| status.state != CodeIntelState::Unavailable,
    )
    .await;
    assert!(
        matches!(
            status.state,
            CodeIntelState::Starting | CodeIntelState::Indexing | CodeIntelState::Ready
        ),
        "valid configured path should restart discovery without an app restart, got {status:?}"
    );
    assert!(
        status
            .message
            .as_deref()
            .map(|message| !message.contains(&missing_path_text))
            .unwrap_or(true),
        "hot-reloaded status must not keep the old bad-path message, got {status:?}"
    );
}
