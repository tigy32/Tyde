mod fixture;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentBootstrapEvent, AgentId, AgentStartPayload, BackendKind, ChatEvent, CommandErrorPayload,
    DiffContextMode, Envelope, FrameKind, HostSettingValue, HostSettingsPayload, MessageOrigin,
    MessageSender, NewAgentPayload, Project, ProjectBootstrapPayload, ProjectCreatePayload,
    ProjectDiffScope, ProjectEventPayload, ProjectGitDiffLineKind, ProjectGitDiffPayload,
    ProjectNotifyPayload, ProjectRootPath, QueuedMessagesPayload, Review, ReviewActionPayload,
    ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewAnchor, ReviewBootstrapPayload,
    ReviewCommentId, ReviewCommentSource, ReviewCreatePayload, ReviewDiffSelection, ReviewDiffSide,
    ReviewErrorCode, ReviewEventPayload, ReviewId, ReviewLocation, ReviewSeverity, ReviewStatus,
    ReviewSubmitTarget, ReviewSubscribePayload, ReviewSuggestedComment, ReviewSuggestionState,
    ReviewSummaryScope, SessionId, SessionListPayload, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::json;

async fn next_env(client: &mut client::Connection, context: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(10), client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn expect_project(client: &mut client::Connection, context: &str) -> Project {
    loop {
        let env = next_env(client, context).await;
        if env.kind != FrameKind::ProjectNotify || !env.stream.0.starts_with("/host/") {
            continue;
        }
        match env
            .parse_payload::<ProjectNotifyPayload>()
            .expect("project notify")
        {
            ProjectNotifyPayload::Upsert { project } => return project,
            ProjectNotifyPayload::Delete { .. } => continue,
        }
    }
}

async fn expect_project_bootstrap(
    client: &mut client::Connection,
    project: &Project,
) -> ProjectBootstrapPayload {
    loop {
        let env = next_env(client, "project bootstrap").await;
        if env.kind == FrameKind::ProjectBootstrap
            && env.stream.0 == format!("/project/{}", project.id.0)
        {
            return env.parse_payload().expect("project bootstrap payload");
        }
    }
}

async fn expect_existing_review_create_echo(
    client: &mut client::Connection,
    project: &Project,
    review_id: &ReviewId,
) {
    let mut saw_bootstrap = false;
    let mut saw_list_changed = false;
    while !saw_bootstrap || !saw_list_changed {
        let env = next_env(client, "existing review_create echo").await;
        match env.kind {
            FrameKind::ReviewBootstrap => {
                let bootstrap: ReviewBootstrapPayload =
                    env.parse_payload().expect("review bootstrap payload");
                if bootstrap.review.id == *review_id {
                    saw_bootstrap = true;
                }
            }
            FrameKind::ProjectEvent if env.stream.0 == format!("/project/{}", project.id.0) => {
                match env
                    .parse_payload::<ProjectEventPayload>()
                    .expect("project event payload")
                {
                    ProjectEventPayload::ReviewListChanged { reviews }
                        if reviews.iter().any(|summary| summary.id == *review_id) =>
                    {
                        saw_list_changed = true;
                    }
                    ProjectEventPayload::ReviewListChanged { .. } => {}
                }
            }
            _ => {}
        }
    }
}

async fn expect_review_summary_update(
    client: &mut client::Connection,
    project: &Project,
    review_id: &ReviewId,
    context: &str,
) -> protocol::ReviewSummary {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::ProjectEvent
            && env.stream.0 == format!("/project/{}", project.id.0)
        {
            let ProjectEventPayload::ReviewListChanged { reviews } =
                env.parse_payload().expect("project event payload");
            if let Some(summary) = reviews.into_iter().find(|summary| summary.id == *review_id) {
                return summary;
            }
        }
    }
}

async fn expect_new_agent(client: &mut client::Connection, context: &str) -> NewAgentPayload {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::NewAgent {
            return env.parse_payload().expect("new agent payload");
        }
    }
}

async fn expect_review_event(client: &mut client::Connection, context: &str) -> ReviewEventPayload {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::ReviewEvent {
            return env.parse_payload().expect("review event payload");
        }
    }
}

async fn expect_review_delta(client: &mut client::Connection, context: &str) -> ReviewEventPayload {
    match expect_review_event(client, context).await {
        ReviewEventPayload::Snapshot { review } => panic!(
            "review mutation emitted unexpected Snapshot for review {} while waiting for {}",
            review.id.0, context
        ),
        event => event,
    }
}

async fn assert_no_trailing_review_snapshot(client: &mut client::Connection, context: &str) {
    const QUIET_FOR: Duration = Duration::from_millis(75);
    const MAX_WAIT: Duration = Duration::from_millis(250);

    let start = tokio::time::Instant::now();
    let max_deadline = start + MAX_WAIT;
    let mut quiet_deadline = start + QUIET_FOR;

    loop {
        let now = tokio::time::Instant::now();
        if now >= quiet_deadline || now >= max_deadline {
            return;
        }
        let deadline = if quiet_deadline <= max_deadline {
            quiet_deadline
        } else {
            max_deadline
        };
        let wait_for = deadline.saturating_duration_since(now);

        match tokio::time::timeout(wait_for, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(Some(env))) => {
                if env.kind == FrameKind::ReviewEvent
                    && let ReviewEventPayload::Snapshot { review } = env
                        .parse_payload::<ReviewEventPayload>()
                        .expect("review event payload")
                {
                    panic!(
                        "review mutation emitted trailing Snapshot for review {} after {}",
                        review.id.0, context
                    );
                }
                quiet_deadline = tokio::time::Instant::now() + QUIET_FOR;
            }
            Ok(Ok(None)) => panic!("connection closed while checking {context}"),
            Ok(Err(err)) => panic!("next_event failed while checking {context}: {err:?}"),
        }
    }
}

async fn assert_no_ai_review_spawned(client: &mut client::Connection, context: &str) {
    const QUIET_FOR: Duration = Duration::from_millis(100);
    const MAX_WAIT: Duration = Duration::from_millis(300);

    let start = tokio::time::Instant::now();
    let max_deadline = start + MAX_WAIT;
    let mut quiet_deadline = start + QUIET_FOR;

    loop {
        let now = tokio::time::Instant::now();
        if now >= quiet_deadline || now >= max_deadline {
            return;
        }
        let deadline = if quiet_deadline <= max_deadline {
            quiet_deadline
        } else {
            max_deadline
        };
        let wait_for = deadline.saturating_duration_since(now);

        match tokio::time::timeout(wait_for, client.next_event()).await {
            Err(_) => return,
            Ok(Ok(Some(env))) => {
                match env.kind {
                    FrameKind::NewAgent => {
                        let payload: NewAgentPayload =
                            env.parse_payload().expect("new agent payload");
                        assert_ne!(
                            payload.name, "AI Review",
                            "clean StartAiReview spawned an AI Review agent during {context}"
                        );
                    }
                    FrameKind::ReviewEvent => {
                        let event: ReviewEventPayload =
                            env.parse_payload().expect("review event payload");
                        if let ReviewEventPayload::AiReviewerChanged { state } = event
                            && state.status == ReviewAiReviewerStatus::Running
                        {
                            panic!("clean StartAiReview entered Running state during {context}");
                        }
                    }
                    _ => {}
                }
                quiet_deadline = tokio::time::Instant::now() + QUIET_FOR;
            }
            Ok(Ok(None)) => panic!("connection closed while checking {context}"),
            Ok(Err(err)) => panic!("next_event failed while checking {context}: {err:?}"),
        }
    }
}

async fn expect_review_error(
    client: &mut client::Connection,
    context: &str,
    code: ReviewErrorCode,
) -> protocol::ReviewErrorPayload {
    match expect_review_delta(client, context).await {
        ReviewEventPayload::Error { error } => {
            assert_eq!(error.code, code);
            error
        }
        other => panic!("expected review error {code:?}, got {other:?}"),
    }
}

async fn expect_host_settings(
    client: &mut client::Connection,
    context: &str,
) -> HostSettingsPayload {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::HostSettings {
            return env.parse_payload().expect("host settings payload");
        }
    }
}

async fn set_default_backend(client: &mut client::Connection, backend_kind: BackendKind) {
    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![backend_kind],
            },
        })
        .await
        .expect("enable backend");
    let settings = expect_host_settings(client, "enabled backend host settings").await;
    assert!(settings.settings.enabled_backends.contains(&backend_kind));

    client
        .set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(backend_kind),
            },
        })
        .await
        .expect("set default backend");
    let settings = expect_host_settings(client, "default backend host settings").await;
    assert_eq!(settings.settings.default_backend, Some(backend_kind));
}

async fn subscribe_review_with_payload(
    client: &mut client::Connection,
    review_id: &ReviewId,
    payload: ReviewSubscribePayload,
) -> Review {
    client
        .review_subscribe(review_id, payload)
        .await
        .expect("review subscribe");
    loop {
        let env = next_env(client, "review subscribe bootstrap").await;
        if env.kind == FrameKind::ReviewBootstrap {
            let bootstrap: ReviewBootstrapPayload =
                env.parse_payload().expect("review bootstrap payload");
            return bootstrap.review;
        }
        if env.kind == FrameKind::CommandError {
            let error: CommandErrorPayload = env.parse_payload().expect("command error payload");
            panic!("review subscribe command error: {error:?}");
        }
    }
}

async fn subscribe_review(client: &mut client::Connection, review_id: &ReviewId) -> Review {
    subscribe_review_with_payload(client, review_id, ReviewSubscribePayload::default()).await
}

async fn create_project(client: &mut client::Connection, root: &Path) -> Project {
    create_project_with_roots(client, vec![root.to_string_lossy().to_string()]).await
}

async fn create_project_with_roots(client: &mut client::Connection, roots: Vec<String>) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: "Review Project".to_owned(),
            roots: roots.into_iter().map(ProjectRootPath).collect(),
        })
        .await
        .expect("project_create");
    expect_project(client, "project create").await
}

fn project_roots(project: &Project) -> Vec<String> {
    project
        .root_paths()
        .into_iter()
        .map(|root| root.0)
        .collect()
}

async fn spawn_project_agent(
    client: &mut client::Connection,
    project: &Project,
) -> (NewAgentPayload, SessionId) {
    spawn_project_agent_with_prompt(client, project, "start review origin", true).await
}

async fn spawn_project_agent_with_prompt(
    client: &mut client::Connection,
    project: &Project,
    prompt: &str,
    wait_until_idle: bool,
) -> (NewAgentPayload, SessionId) {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Review Origin".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: project_roots(project),
                prompt: prompt.to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("spawn agent");
    let new_agent = expect_new_agent(client, "new origin agent").await;
    let mut saw_start = false;
    let mut saw_idle = !wait_until_idle;
    let mut session_id = None;
    while !saw_start || !saw_idle || session_id.is_none() {
        let env = next_env(client, "origin agent startup").await;
        match env.kind {
            FrameKind::AgentBootstrap if env.stream == new_agent.instance_stream => {
                let bootstrap: protocol::AgentBootstrapPayload =
                    env.parse_payload().expect("agent bootstrap payload");
                for event in bootstrap.events {
                    match event {
                        AgentBootstrapEvent::AgentStart(_) => {
                            saw_start = true;
                        }
                        AgentBootstrapEvent::ChatEvent(ChatEvent::TypingStatusChanged(false)) => {
                            saw_idle = true;
                        }
                        _ => {}
                    }
                }
            }
            FrameKind::AgentStart if env.stream == new_agent.instance_stream => {
                let _: AgentStartPayload = env.parse_payload().expect("agent start payload");
                saw_start = true;
            }
            FrameKind::ChatEvent if env.stream == new_agent.instance_stream => {
                let event: ChatEvent = env.parse_payload().expect("chat event");
                if matches!(event, ChatEvent::TypingStatusChanged(false)) {
                    saw_idle = true;
                }
            }
            FrameKind::SessionList => {
                let payload: SessionListPayload = env.parse_payload().expect("session list");
                if let Some(session) = payload.sessions.into_iter().next() {
                    session_id = Some(session.id);
                }
            }
            _ => {}
        }
    }
    let session_id = session_id.expect("session id must be set");
    (new_agent, session_id)
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to spawn: {err}", args));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed_repo(root: &Path) {
    git(root, &["init"]);
    git(root, &["config", "user.email", "review@example.com"]);
    git(root, &["config", "user.name", "Review Test"]);
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(root.join("src/lib.rs"), "fn value() -> i32 {\n    1\n}\n").expect("write file");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "Initial"]);
    fs::write(
        root.join("src/lib.rs"),
        "fn value() -> i32 {\n    1\n}\n\nfn extra() -> i32 {\n    2\n}\n",
    )
    .expect("modify file");
}

fn new_line_location(review: &Review) -> ReviewLocation {
    let diff = review
        .diffs
        .iter()
        .find(|diff| diff.root.0.ends_with("review-root"))
        .or_else(|| review.diffs.first())
        .expect("review diff");
    let file = diff
        .files
        .iter()
        .find(|file| file.relative_path == "src/lib.rs")
        .expect("src/lib.rs diff");
    let added_line = file
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .find(|line| line.kind == ProjectGitDiffLineKind::Added)
        .expect("added line");
    ReviewLocation {
        root: diff.root.clone(),
        relative_path: file.relative_path.clone(),
        anchor: ReviewAnchor::LineRange {
            side: ReviewDiffSide::New,
            start_line: added_line.new_line_number.expect("new line number"),
            end_line: added_line.new_line_number.expect("new line number"),
        },
    }
}

fn new_line_location_for_root(review: &Review, root: &str) -> ReviewLocation {
    let diff = review
        .diffs
        .iter()
        .find(|diff| diff.root.0 == root)
        .unwrap_or_else(|| panic!("review diff for root {root}"));
    let file = diff
        .files
        .iter()
        .find(|file| file.relative_path == "src/lib.rs")
        .unwrap_or_else(|| panic!("src/lib.rs diff for root {root}"));
    let added_line = file
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .find(|line| line.kind == ProjectGitDiffLineKind::Added)
        .unwrap_or_else(|| panic!("added line for root {root}"));
    ReviewLocation {
        root: diff.root.clone(),
        relative_path: file.relative_path.clone(),
        anchor: ReviewAnchor::LineRange {
            side: ReviewDiffSide::New,
            start_line: added_line.new_line_number.expect("new line number"),
            end_line: added_line.new_line_number.expect("new line number"),
        },
    }
}

fn out_of_range_location(review: &Review) -> ReviewLocation {
    let mut location = new_line_location(review);
    location.anchor = ReviewAnchor::LineRange {
        side: ReviewDiffSide::New,
        start_line: 999,
        end_line: 999,
    };
    location
}

fn wrong_side_location(review: &Review) -> ReviewLocation {
    let mut location = new_line_location(review);
    if let ReviewAnchor::LineRange {
        start_line,
        end_line,
        ..
    } = location.anchor
    {
        location.anchor = ReviewAnchor::LineRange {
            side: ReviewDiffSide::Old,
            start_line,
            end_line,
        };
    }
    location
}

fn sample_stored_review(
    id: &str,
    project: &Project,
    root: &Path,
    status: ReviewStatus,
    ai_status: ReviewAiReviewerStatus,
) -> Review {
    Review {
        id: ReviewId(id.to_owned()),
        project_id: project.id.clone(),
        origin_agent_id: AgentId("550e8400-e29b-41d4-a716-446655440001".to_owned()),
        origin_session_id: SessionId("stored-session".to_owned()),
        selection: ReviewDiffSelection::Root {
            root: ProjectRootPath(root.to_string_lossy().to_string()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        },
        status,
        diffs: vec![ProjectGitDiffPayload {
            root: ProjectRootPath(root.to_string_lossy().to_string()),
            scope: ProjectDiffScope::Unstaged,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: Vec::new(),
        }],
        comments: Vec::new(),
        suggestions: Vec::<ReviewSuggestedComment>::new(),
        ai_reviewer: ReviewAiReviewerState {
            status: ai_status,
            agent_id: (ai_status == ReviewAiReviewerStatus::Running)
                .then(|| AgentId("550e8400-e29b-41d4-a716-446655440002".to_owned())),
            error: (ai_status == ReviewAiReviewerStatus::Running)
                .then(|| "stale running reviewer".to_owned()),
        },
        created_at_ms: 1,
        updated_at_ms: 2,
    }
}

async fn create_review(
    client: &mut client::Connection,
    project: &Project,
    _origin: &NewAgentPayload,
) -> Review {
    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                selection: ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
            },
        )
        .await
        .expect("review create");
    loop {
        let env = next_env(client, "review bootstrap").await;
        if env.kind == FrameKind::ReviewBootstrap {
            let bootstrap: ReviewBootstrapPayload =
                env.parse_payload().expect("review bootstrap payload");
            return bootstrap.review;
        }
    }
}

async fn create_review_for_root(
    client: &mut client::Connection,
    project: &Project,
    root: &str,
) -> Review {
    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                selection: ReviewDiffSelection::Root {
                    root: ProjectRootPath(root.to_owned()),
                    scope: ProjectDiffScope::Unstaged,
                    path: None,
                },
            },
        )
        .await
        .expect("review create");
    loop {
        let env = next_env(client, "review bootstrap").await;
        if env.kind == FrameKind::ReviewBootstrap {
            let bootstrap: ReviewBootstrapPayload =
                env.parse_payload().expect("review bootstrap payload");
            return bootstrap.review;
        }
    }
}

fn submit_to(agent: &NewAgentPayload) -> ReviewActionPayload {
    ReviewActionPayload::Submit {
        target: ReviewSubmitTarget::ExistingAgent {
            agent_id: agent.agent_id.clone(),
        },
    }
}

async fn add_comment(
    client: &mut client::Connection,
    review: &Review,
    body: &str,
) -> ReviewCommentId {
    let location = new_line_location(review);
    client
        .review_action(
            &review.id,
            ReviewActionPayload::AddComment {
                location,
                body: body.to_owned(),
            },
        )
        .await
        .expect("add comment");
    let comment_id = match expect_review_delta(client, "comment upsert delta").await {
        ReviewEventPayload::CommentUpsert { comment } => comment.id,
        other => panic!("expected comment upsert, got {other:?}"),
    };
    assert_no_trailing_review_snapshot(client, "AddComment delta").await;
    comment_id
}

async fn call_propose_review_comment_tool(
    fixture: &Fixture,
    reviewer_agent_id: &AgentId,
    review_id: &ReviewId,
    location: ReviewLocation,
) -> serde_json::Value {
    let base_url = fixture.review_mcp_http_url().await;
    let separator = if base_url.contains('?') { '&' } else { '?' };
    let url = format!("{base_url}{separator}agent_id={}", reviewer_agent_id.0);
    let transport = StreamableHttpClientTransport::from_uri(url);
    let service = ().serve(transport).await.expect("connect to review MCP");
    let arguments = json!({
        "review_id": review_id,
        "location": location,
        "body": "AI found a review issue.",
        "severity": "bug",
        "rationale": "The changed value needs attention."
    })
    .as_object()
    .cloned();
    let result = service
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "propose_review_comment".into(),
            arguments,
            task: None,
        })
        .await
        .expect("call propose_review_comment");
    assert_eq!(result.is_error, Some(false));
    let content = result
        .content
        .first()
        .expect("tool result should include content");
    let RawContent::Text(text) = &content.raw else {
        panic!("expected text JSON tool result, got {:?}", content.raw);
    };
    let value: serde_json::Value =
        serde_json::from_str(&text.text).expect("tool result text must be JSON");
    service.cancel().await.expect("cancel MCP client");
    value
}

async fn close_agent_and_wait(client: &mut client::Connection, stream: &protocol::StreamPath) {
    client.close_agent(stream).await.expect("close agent");
    loop {
        let env = next_env(client, "agent closed").await;
        if env.kind == FrameKind::AgentClosed {
            break;
        }
    }
}

fn tyde_review_json(markdown: &str) -> &str {
    let fence = "```tyde-review";
    let start = markdown
        .find(fence)
        .expect("markdown should include tyde-review fence")
        + fence.len();
    let rest = markdown[start..].trim_start_matches(['\r', '\n']);
    let end = rest.find("\n```").expect("tyde-review fence should close");
    &rest[..end]
}

#[tokio::test]
async fn project_bootstrap_exposes_one_active_workspace_review() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    set_default_backend(&mut client, BackendKind::Claude).await;
    let root = tempfile::tempdir().expect("temp root");
    let repo_a = root.path().join("review-root-a");
    let repo_b = root.path().join("review-root-b");
    fs::create_dir_all(&repo_a).expect("create repo a");
    fs::create_dir_all(&repo_b).expect("create repo b");
    seed_repo(&repo_a);
    seed_repo(&repo_b);

    let project = create_project_with_roots(
        &mut client,
        vec![
            repo_a.to_string_lossy().to_string(),
            repo_b.to_string_lossy().to_string(),
        ],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut client, &project).await;

    assert_eq!(bootstrap.review_summaries.len(), 1);
    let summary = &bootstrap.review_summaries[0];
    assert_eq!(summary.scope, ReviewSummaryScope::Workspace);
    assert!(matches!(summary.status, ReviewStatus::Draft));

    let review = subscribe_review(&mut client, &summary.id).await;
    assert_eq!(review.project_id, project.id);
    assert_eq!(
        review.selection,
        ReviewDiffSelection::Workspace {
            scope: ProjectDiffScope::Unstaged,
        }
    );
    assert_eq!(review.diffs.len(), 2);
    let diff_roots = review
        .diffs
        .iter()
        .map(|diff| diff.root.0.as_str())
        .collect::<Vec<_>>();
    assert!(diff_roots.contains(&project_roots(&project)[0].as_str()));
    assert!(diff_roots.contains(&project_roots(&project)[1].as_str()));
    assert!(
        review
            .diffs
            .iter()
            .all(|diff| diff.scope == ProjectDiffScope::Unstaged)
    );

    client
        .review_action(
            &summary.id,
            ReviewActionPayload::StartAiReview {
                backend_kind: None,
                cost_hint: None,
                instructions: Some("__mock_slow__ Check both roots.".to_owned()),
            },
        )
        .await
        .expect("start workspace AI review");

    let mut new_agent = None;
    let mut running = None;
    while new_agent.is_none() || running.is_none() {
        let env = next_env(&mut client, "workspace AI reviewer start").await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env.parse_payload().expect("new agent payload");
                assert_eq!(payload.name, "AI Review");
                assert_eq!(payload.project_id, Some(project.id.clone()));
                assert_eq!(payload.workspace_roots, project_roots(&project));
                assert!(
                    new_agent.replace(payload).is_none(),
                    "expected one NewAgent"
                );
            }
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event payload") {
                ReviewEventPayload::AiReviewerChanged { state }
                    if state.status == ReviewAiReviewerStatus::Running =>
                {
                    assert!(
                        running.replace(state).is_none(),
                        "expected one running AI reviewer event"
                    );
                }
                ReviewEventPayload::Snapshot { review } => {
                    panic!(
                        "unexpected Snapshot for review {} while waiting for workspace AI reviewer",
                        review.id.0
                    );
                }
                _ => {}
            },
            _ => {}
        }
    }
    let new_agent = new_agent.expect("new AI Review agent");
    let running = running.expect("running AI reviewer state");
    assert_eq!(running.agent_id, Some(new_agent.agent_id.clone()));
    close_agent_and_wait(&mut client, &new_agent.instance_stream).await;
}

#[tokio::test]
async fn start_ai_review_on_clean_workspace_errors_without_spawning_agent() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    set_default_backend(&mut client, BackendKind::Claude).await;
    let root = tempfile::tempdir().expect("temp root");
    let repo_a = root.path().join("clean-review-root-a");
    let repo_b = root.path().join("clean-review-root-b");
    fs::create_dir_all(&repo_a).expect("create repo a");
    fs::create_dir_all(&repo_b).expect("create repo b");
    seed_repo(&repo_a);
    seed_repo(&repo_b);
    git(&repo_a, &["add", "."]);
    git(&repo_a, &["commit", "-m", "Apply changes"]);
    git(&repo_b, &["add", "."]);
    git(&repo_b, &["commit", "-m", "Apply changes"]);

    let project = create_project_with_roots(
        &mut client,
        vec![
            repo_a.to_string_lossy().to_string(),
            repo_b.to_string_lossy().to_string(),
        ],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut client, &project).await;
    assert_eq!(bootstrap.review_summaries.len(), 1);
    let review_id = bootstrap.review_summaries[0].id.clone();
    let review = subscribe_review(&mut client, &review_id).await;
    assert!(review.diffs.is_empty());
    assert_eq!(review.ai_reviewer.status, ReviewAiReviewerStatus::Idle);

    client
        .review_action(
            &review.id,
            ReviewActionPayload::StartAiReview {
                backend_kind: None,
                cost_hint: None,
                instructions: Some("There should be nothing to review.".to_owned()),
            },
        )
        .await
        .expect("start AI review on clean workspace");

    let mut saw_error = false;
    while !saw_error {
        let env = next_env(&mut client, "clean workspace StartAiReview").await;
        match env.kind {
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env.parse_payload().expect("new agent payload");
                assert_ne!(
                    payload.name, "AI Review",
                    "clean StartAiReview must not spawn an AI Review agent"
                );
            }
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event payload") {
                ReviewEventPayload::Error { error } => {
                    assert_eq!(error.code, ReviewErrorCode::InvalidStatus);
                    assert!(matches!(
                        error.context,
                        protocol::ReviewErrorContext::StartAiReview
                    ));
                    assert!(
                        error.message.contains("nothing to review"),
                        "unexpected clean StartAiReview error: {}",
                        error.message
                    );
                    saw_error = true;
                }
                ReviewEventPayload::AiReviewerChanged { state }
                    if state.status == ReviewAiReviewerStatus::Running =>
                {
                    panic!("clean StartAiReview must not enter Running state");
                }
                ReviewEventPayload::Cleared { review: cleared } => {
                    assert_ne!(cleared.ai_reviewer.status, ReviewAiReviewerStatus::Running);
                }
                _ => {}
            },
            _ => {}
        }
    }
    assert_no_ai_review_spawned(&mut client, "clean StartAiReview").await;

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert_ne!(snapshot.ai_reviewer.status, ReviewAiReviewerStatus::Running);
    assert_eq!(snapshot.ai_reviewer.agent_id, None);
}

#[tokio::test]
async fn create_review_add_update_delete_and_submit_live() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;

    assert_eq!(review.diffs.len(), 1);
    assert_eq!(review.diffs[0].scope, ProjectDiffScope::Unstaged);
    assert_eq!(review.diffs[0].context_mode, DiffContextMode::FullFile);

    let comment_id = add_comment(&mut client, &review, "Please handle this change.").await;
    client
        .review_action(
            &review.id,
            ReviewActionPayload::UpdateComment {
                comment_id: comment_id.clone(),
                body: "Updated comment.".to_owned(),
            },
        )
        .await
        .expect("update comment");
    match expect_review_delta(&mut client, "updated comment delta").await {
        ReviewEventPayload::CommentUpsert { comment } => {
            assert_eq!(comment.id, comment_id);
            assert_eq!(comment.body, "Updated comment.");
        }
        other => panic!("expected updated comment, got {other:?}"),
    }
    assert_no_trailing_review_snapshot(&mut client, "UpdateComment delta").await;

    client
        .review_action(
            &review.id,
            ReviewActionPayload::DeleteComment {
                comment_id: comment_id.clone(),
            },
        )
        .await
        .expect("delete comment");
    match expect_review_delta(&mut client, "deleted comment delta").await {
        ReviewEventPayload::CommentDelete { comment_id: id } => assert_eq!(id, comment_id),
        other => panic!("expected comment delete, got {other:?}"),
    }
    assert_no_trailing_review_snapshot(&mut client, "DeleteComment delta").await;

    let _comment_id = add_comment(&mut client, &review, "Final review comment.").await;
    client
        .review_action(&review.id, submit_to(&agent))
        .await
        .expect("submit review");
    match expect_review_delta(&mut client, "submit cleared delta").await {
        ReviewEventPayload::Cleared { review: cleared } => {
            assert_eq!(cleared.id, review.id);
            assert!(matches!(cleared.status, ReviewStatus::Draft));
            assert!(cleared.comments.is_empty());
            assert!(cleared.suggestions.is_empty());
            assert_eq!(cleared.ai_reviewer.status, ReviewAiReviewerStatus::Idle);
        }
        other => panic!("expected cleared review after submit, got {other:?}"),
    }
}

#[tokio::test]
async fn workspace_review_counts_submit_and_clean_reset_across_roots() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo_a = root.path().join("review-root-a");
    let repo_b = root.path().join("review-root-b");
    fs::create_dir_all(&repo_a).expect("create repo a");
    fs::create_dir_all(&repo_b).expect("create repo b");
    seed_repo(&repo_a);
    seed_repo(&repo_b);

    let project = create_project_with_roots(
        &mut client,
        vec![
            repo_a.to_string_lossy().to_string(),
            repo_b.to_string_lossy().to_string(),
        ],
    )
    .await;
    let bootstrap = expect_project_bootstrap(&mut client, &project).await;
    assert_eq!(bootstrap.review_summaries.len(), 1);
    let review_id = bootstrap.review_summaries[0].id.clone();
    let review = subscribe_review(&mut client, &review_id).await;
    let location_a = new_line_location_for_root(&review, &project_roots(&project)[0]);
    let location_b = new_line_location_for_root(&review, &project_roots(&project)[1]);
    let (agent, _session_id) = spawn_project_agent_with_prompt(
        &mut client,
        &project,
        "start review target __mock_slow__",
        false,
    )
    .await;

    for (location, body) in [
        (location_a.clone(), "Root A review comment."),
        (location_b.clone(), "Root B review comment."),
    ] {
        client
            .review_action(
                &review.id,
                ReviewActionPayload::AddComment {
                    location,
                    body: body.to_owned(),
                },
            )
            .await
            .expect("add workspace comment");
        match expect_review_delta(&mut client, "workspace comment upsert").await {
            ReviewEventPayload::CommentUpsert { comment } => assert_eq!(comment.body, body),
            other => panic!("expected workspace comment upsert, got {other:?}"),
        }
    }

    let summary = loop {
        let summary =
            expect_review_summary_update(&mut client, &project, &review.id, "workspace counts")
                .await;
        if summary.file_comment_counts.len() == 2 {
            break summary;
        }
    };
    assert_eq!(summary.scope, ReviewSummaryScope::Workspace);
    for root in &project_roots(&project) {
        let count = summary
            .file_comment_counts
            .iter()
            .find(|count| count.root.0 == *root && count.relative_path == "src/lib.rs")
            .unwrap_or_else(|| panic!("missing comment count for root {root}"));
        assert_eq!(count.user_comment_count, 1);
        assert_eq!(count.ai_comment_count, 0);
        assert_eq!(count.pending_suggestion_count, 0);
        assert_eq!(count.total_count(), 1);
    }

    client
        .review_action(&review.id, submit_to(&agent))
        .await
        .expect("submit workspace review");

    let mut cleared_count = 0;
    let mut queued_review_message = None;
    while cleared_count == 0 || queued_review_message.is_none() {
        let env = next_env(&mut client, "workspace review submit").await;
        match env.kind {
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event") {
                ReviewEventPayload::Cleared { review: cleared } => {
                    assert_eq!(cleared.id, review.id);
                    assert!(cleared.comments.is_empty());
                    cleared_count += 1;
                }
                other => panic!("unexpected review event during workspace submit: {other:?}"),
            },
            FrameKind::QueuedMessages if env.stream == agent.instance_stream => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("queued messages payload");
                let review_messages = payload
                    .messages
                    .iter()
                    .filter(|entry| {
                        entry.origin
                            == Some(MessageOrigin::Review {
                                review_id: review.id.clone(),
                            })
                    })
                    .collect::<Vec<_>>();
                if !review_messages.is_empty() {
                    assert_eq!(review_messages.len(), 1);
                    queued_review_message = Some(review_messages[0].message.clone());
                }
            }
            _ => {}
        }
    }
    assert_eq!(cleared_count, 1);
    let queued_review_message = queued_review_message.expect("queued review message");
    let bundle: serde_json::Value = serde_json::from_str(tyde_review_json(&queued_review_message))
        .expect("workspace feedback bundle JSON");
    assert_eq!(bundle["review_id"], review.id.0);
    let comments = bundle["comments"].as_array().expect("comments array");
    assert_eq!(comments.len(), 2);
    let bundle_roots = comments
        .iter()
        .map(|comment| comment["location"]["root"].as_str().expect("root"))
        .collect::<Vec<_>>();
    assert!(bundle_roots.contains(&project_roots(&project)[0].as_str()));
    assert!(bundle_roots.contains(&project_roots(&project)[1].as_str()));

    for (location, body) in [
        (location_a.clone(), "Root A reset comment."),
        (location_b.clone(), "Root B reset comment."),
    ] {
        client
            .review_action(
                &review.id,
                ReviewActionPayload::AddComment {
                    location,
                    body: body.to_owned(),
                },
            )
            .await
            .expect("add reset comment");
        match expect_review_delta(&mut client, "reset comment upsert").await {
            ReviewEventPayload::CommentUpsert { comment } => assert_eq!(comment.body, body),
            other => panic!("expected reset comment upsert, got {other:?}"),
        }
    }

    git(&repo_a, &["add", "."]);
    git(&repo_a, &["commit", "-m", "Apply root A"]);
    let partial_clean = subscribe_review(&mut client, &review.id).await;
    assert_eq!(
        partial_clean.comments.len(),
        2,
        "one clean root must not clear the workspace review while another root is dirty"
    );
    let root_a_comment = partial_clean
        .comments
        .iter()
        .find(|comment| comment.location.root.0 == project_roots(&project)[0])
        .expect("root A comment");
    assert!(matches!(
        root_a_comment.anchor_status,
        protocol::ReviewAnchorStatus::Stale { .. }
    ));
    assert!(
        partial_clean
            .diffs
            .iter()
            .any(|diff| diff.root.0 == project_roots(&project)[1] && !diff.files.is_empty())
    );

    git(&repo_b, &["add", "."]);
    git(&repo_b, &["commit", "-m", "Apply root B"]);
    let all_clean = subscribe_review(&mut client, &review.id).await;
    assert!(all_clean.comments.is_empty());
    assert!(all_clean.suggestions.is_empty());
    assert_eq!(all_clean.ai_reviewer.status, ReviewAiReviewerStatus::Idle);
    assert!(all_clean.diffs.is_empty());
}

#[tokio::test]
async fn review_subscribe_include_diffs_controls_bootstrap_and_cleared_payloads() {
    let fixture = Fixture::new().await;
    let mut client = fixture.connect().await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    assert_eq!(
        review.diffs.len(),
        1,
        "review_create remains a full subscriber"
    );

    let mut lightweight = fixture.connect().await;
    let redacted = subscribe_review_with_payload(
        &mut lightweight,
        &review.id,
        ReviewSubscribePayload {
            include_diffs: false,
        },
    )
    .await;
    assert_eq!(redacted.id, review.id);
    assert!(
        redacted.diffs.is_empty(),
        "include_diffs=false must redact ReviewBootstrap diffs"
    );

    lightweight
        .review_action(&review.id, ReviewActionPayload::ClearComments)
        .await
        .expect("clear comments");
    match expect_review_event(&mut lightweight, "lightweight cleared event").await {
        ReviewEventPayload::Cleared { review } => {
            assert_eq!(review.id, redacted.id);
            assert!(
                review.diffs.is_empty(),
                "include_diffs=false must redact Cleared review diffs"
            );
        }
        other => panic!("expected cleared review, got {other:?}"),
    }

    let mut legacy = fixture.connect().await;
    let full = subscribe_review(&mut legacy, &review.id).await;
    assert_eq!(
        full.diffs.len(),
        1,
        "default legacy {{}} subscribe must keep full diffs"
    );
}

#[tokio::test]
async fn review_subscribe_can_upgrade_to_full_but_not_downgrade() {
    let fixture = Fixture::new().await;
    let mut client = fixture.connect().await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;

    let mut subscriber = fixture.connect().await;
    let redacted = subscribe_review_with_payload(
        &mut subscriber,
        &review.id,
        ReviewSubscribePayload {
            include_diffs: false,
        },
    )
    .await;
    assert!(redacted.diffs.is_empty());

    let upgraded = subscribe_review(&mut subscriber, &review.id).await;
    assert_eq!(
        upgraded.diffs.len(),
        1,
        "default subscribe should upgrade a lightweight subscriber to full"
    );

    let still_full = subscribe_review_with_payload(
        &mut subscriber,
        &review.id,
        ReviewSubscribePayload {
            include_diffs: false,
        },
    )
    .await;
    assert_eq!(
        still_full.diffs.len(),
        1,
        "a full subscriber should not be downgraded by a later lightweight subscribe"
    );
}

#[tokio::test]
async fn lightweight_review_subscribe_skips_full_root_diff_refresh() {
    let fixture = Fixture::new().await;
    let mut client = fixture.connect().await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    assert_eq!(review.diffs.len(), 1);

    let moved_repo = root.path().join("review-root-moved");
    fs::rename(&repo, &moved_repo).expect("move repo out from under project root");

    let mut lightweight = fixture.connect().await;
    let redacted = subscribe_review_with_payload(
        &mut lightweight,
        &review.id,
        ReviewSubscribePayload {
            include_diffs: false,
        },
    )
    .await;
    assert_eq!(redacted.id, review.id);
    assert!(
        redacted.diffs.is_empty(),
        "lightweight subscribe should bootstrap without refreshing missing root diffs"
    );
}

#[tokio::test]
async fn root_scoped_review_create_uses_selected_project_root() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let mut git_roots = Vec::new();
    for index in 0..4 {
        let repo = root.path().join(format!("git-root-{index}"));
        fs::create_dir_all(&repo).expect("create repo");
        seed_repo(&repo);
        git_roots.push(repo);
    }
    let plain_root = root.path().join("plain-root");
    fs::create_dir_all(&plain_root).expect("create plain root");
    fs::write(plain_root.join("notes.txt"), "not a git checkout\n").expect("write plain file");
    let plain_root = plain_root.to_string_lossy().to_string();

    let project_roots = vec![
        git_roots[0].to_string_lossy().to_string(),
        git_roots[1].to_string_lossy().to_string(),
        plain_root.clone(),
        git_roots[2].to_string_lossy().to_string(),
        git_roots[3].to_string_lossy().to_string(),
    ];

    let project = create_project_with_roots(&mut client, project_roots).await;
    let (_agent, _session_id) = spawn_project_agent(&mut client, &project).await;

    for git_root in &git_roots {
        let git_root = git_root.to_string_lossy();
        let review = create_review_for_root(&mut client, &project, &git_root).await;
        assert_eq!(review.diffs.len(), 1);
        let diff = review
            .diffs
            .iter()
            .find(|diff| diff.root.0 == git_root)
            .unwrap_or_else(|| panic!("missing review diff for {git_root}"));
        assert_eq!(diff.scope, ProjectDiffScope::Unstaged);
        assert_eq!(diff.context_mode, DiffContextMode::FullFile);
        assert!(
            diff.files
                .iter()
                .any(|file| file.relative_path == "src/lib.rs"),
            "missing src/lib.rs diff for {git_root}"
        );
    }

    let review = create_review_for_root(&mut client, &project, &plain_root).await;
    assert!(review.diffs.is_empty());
    assert_eq!(
        review.selection,
        ReviewDiffSelection::Root {
            root: ProjectRootPath(plain_root),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        }
    );
}

#[tokio::test]
async fn create_review_with_only_non_git_roots_succeeds_empty() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let plain_a = root.path().join("plain-a");
    let plain_b = root.path().join("plain-b");
    fs::create_dir_all(&plain_a).expect("create plain root a");
    fs::create_dir_all(&plain_b).expect("create plain root b");
    fs::write(plain_a.join("notes.txt"), "not a git checkout\n").expect("write plain file");

    let project = create_project_with_roots(
        &mut client,
        vec![
            plain_a.to_string_lossy().to_string(),
            plain_b.to_string_lossy().to_string(),
        ],
    )
    .await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;

    assert!(review.diffs.is_empty());
}

#[tokio::test]
async fn create_review_does_not_require_origin_agent() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                selection: ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
            },
        )
        .await
        .expect("review create without origin");

    let review = loop {
        let env = next_env(&mut client, "origin-free review bootstrap").await;
        if env.kind == FrameKind::ReviewBootstrap {
            let bootstrap: ReviewBootstrapPayload =
                env.parse_payload().expect("review bootstrap payload");
            break bootstrap.review;
        }
    };
    assert_eq!(review.project_id, project.id);
    assert!(matches!(review.status, ReviewStatus::Draft));
    assert_eq!(review.diffs.len(), 1);
}

#[tokio::test]
async fn create_review_with_untracked_binary_file_allows_file_comment() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "review@example.com"]);
    git(&repo, &["config", "user.name", "Review Test"]);
    fs::write(repo.join("README.md"), "initial\n").expect("write initial file");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "Initial"]);
    fs::write(repo.join("binary.dat"), [0xff_u8, 0xfe_u8, 0x00_u8])
        .expect("write untracked binary file");

    let project = create_project(&mut client, &repo).await;
    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                selection: ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
            },
        )
        .await
        .expect("review create with untracked binary");

    let review = loop {
        let env = next_env(&mut client, "binary review bootstrap").await;
        if env.kind == FrameKind::ReviewBootstrap {
            let bootstrap: ReviewBootstrapPayload =
                env.parse_payload().expect("review bootstrap payload");
            break bootstrap.review;
        }
    };
    let diff = review.diffs.first().expect("binary review diff");
    let binary_file = diff
        .files
        .iter()
        .find(|file| file.relative_path == "binary.dat")
        .expect("binary file diff");
    assert!(binary_file.is_binary);
    assert!(binary_file.hunks.is_empty());

    let location = ReviewLocation {
        root: diff.root.clone(),
        relative_path: "binary.dat".to_owned(),
        anchor: ReviewAnchor::File,
    };
    client
        .review_action(
            &review.id,
            ReviewActionPayload::AddComment {
                location: location.clone(),
                body: "Please check this asset.".to_owned(),
            },
        )
        .await
        .expect("add binary file-level comment");

    match expect_review_delta(&mut client, "binary file comment upsert").await {
        ReviewEventPayload::CommentUpsert { comment } => {
            assert_eq!(comment.location, location);
            assert_eq!(comment.body, "Please check this asset.");
            assert_eq!(comment.source, ReviewCommentSource::User);
        }
        other => panic!("expected binary file comment upsert, got {other:?}"),
    }
    assert_no_trailing_review_snapshot(&mut client, "binary file AddComment delta").await;
}

#[tokio::test]
async fn submitted_review_sends_rendered_markdown_to_origin() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let location = new_line_location(&review);
    let expected_heading = match &location.anchor {
        ReviewAnchor::LineRange {
            start_line,
            end_line,
            ..
        } if start_line == end_line => {
            format!("### {}:{} (new)", location.relative_path, start_line)
        }
        ReviewAnchor::LineRange {
            start_line,
            end_line,
            ..
        } => format!(
            "### {}:{}-{} (new)",
            location.relative_path, start_line, end_line
        ),
        other => panic!("expected line range anchor, got {other:?}"),
    };

    client
        .review_action(
            &review.id,
            ReviewActionPayload::AddComment {
                location: location.clone(),
                body: "fix this please".to_owned(),
            },
        )
        .await
        .expect("add comment");
    match expect_review_delta(&mut client, "comment upsert delta").await {
        ReviewEventPayload::CommentUpsert { .. } => {}
        other => panic!("expected comment upsert, got {other:?}"),
    }

    client
        .review_action(&review.id, submit_to(&agent))
        .await
        .expect("submit review");

    let mut saw_cleared = false;
    let mut delivered_message = None;
    while !saw_cleared || delivered_message.is_none() {
        let env = next_env(&mut client, "rendered review delivery").await;
        match env.kind {
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event") {
                ReviewEventPayload::Cleared { review: cleared } => {
                    assert_eq!(cleared.id, review.id);
                    assert!(cleared.comments.is_empty());
                    saw_cleared = true;
                }
                other => panic!("unexpected review event while waiting for delivery: {other:?}"),
            },
            FrameKind::ChatEvent if env.stream == agent.instance_stream => {
                let event: ChatEvent = env.parse_payload().expect("chat event");
                let message = match event {
                    ChatEvent::MessageAdded(message) => Some(message),
                    ChatEvent::StreamEnd(end) => Some(end.message),
                    _ => None,
                };
                if let Some(message) = message
                    && matches!(message.sender, MessageSender::Assistant { .. })
                    && message.content.contains("```tyde-review")
                {
                    delivered_message = Some(message.content);
                }
            }
            _ => {}
        }
    }

    let delivered_message = delivered_message.expect("review message should be delivered");
    assert!(delivered_message.contains("The user finished a review with 1 comments."));
    assert!(delivered_message.contains("```tyde-review"));
    assert!(delivered_message.contains("fix this please"));
    assert!(delivered_message.contains(&expected_heading));

    let bundle: serde_json::Value =
        serde_json::from_str(tyde_review_json(&delivered_message)).expect("feedback bundle JSON");
    assert_eq!(bundle["review_id"], review.id.0);
    let comments = bundle["comments"].as_array().expect("comments array");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "fix this please");
    assert_eq!(
        comments[0]["location"],
        serde_json::to_value(&location).unwrap()
    );
}

#[tokio::test]
async fn submit_to_closed_existing_agent_keeps_draft_comments() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let comment_id = add_comment(&mut client, &review, "Offline delivery comment.").await;

    close_agent_and_wait(&mut client, &agent.instance_stream).await;

    client
        .review_action(&review.id, submit_to(&agent))
        .await
        .expect("submit to closed agent");
    let error = expect_review_error(
        &mut client,
        "closed target error",
        ReviewErrorCode::InvalidSubmitTarget,
    )
    .await;
    assert!(!error.fatal);
    assert_no_trailing_review_snapshot(&mut client, "closed target Submit error").await;

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(matches!(snapshot.status, ReviewStatus::Draft));
    assert_eq!(snapshot.comments.len(), 1);
    assert_eq!(snapshot.comments[0].id, comment_id);
}

#[tokio::test]
async fn invalid_locations_emit_typed_error_without_mutation() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;

    for location in [out_of_range_location(&review), wrong_side_location(&review)] {
        client
            .review_action(
                &review.id,
                ReviewActionPayload::AddComment {
                    location,
                    body: "invalid".to_owned(),
                },
            )
            .await
            .expect("invalid add comment action");
        let error = expect_review_error(
            &mut client,
            "invalid location error",
            ReviewErrorCode::InvalidLocation,
        )
        .await;
        assert!(!error.fatal);
        assert_no_trailing_review_snapshot(&mut client, "InvalidLocation error").await;
    }

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(snapshot.comments.is_empty());
}

#[tokio::test]
async fn review_resets_when_uncommitted_diff_becomes_clean() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let _comment_id = add_comment(&mut client, &review, "Clean reset comment.").await;

    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "Apply changes"]);

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(matches!(snapshot.status, ReviewStatus::Draft));
    assert!(snapshot.comments.is_empty());
    assert!(snapshot.suggestions.is_empty());
    assert_eq!(snapshot.ai_reviewer.status, ReviewAiReviewerStatus::Idle);
    assert!(snapshot.diffs.is_empty());
}

#[tokio::test]
async fn review_resets_when_unstaged_diff_becomes_clean_with_staged_changes() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let _comment_id = add_comment(&mut client, &review, "Staged reset comment.").await;

    git(&repo, &["add", "."]);

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(matches!(snapshot.status, ReviewStatus::Draft));
    assert!(snapshot.comments.is_empty());
    assert!(snapshot.suggestions.is_empty());
    assert_eq!(snapshot.ai_reviewer.status, ReviewAiReviewerStatus::Idle);
    assert!(snapshot.diffs.is_empty());
}

#[tokio::test]
async fn ai_reviewer_propose_tool_accepts_and_rejects_suggestions() {
    let fixture = Fixture::new().await;
    let mut client = fixture.connect().await;
    set_default_backend(&mut client, BackendKind::Claude).await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let location = new_line_location(&review);

    client
        .review_action(
            &review.id,
            ReviewActionPayload::StartAiReview {
                backend_kind: None,
                cost_hint: None,
                instructions: Some(
                    "__mock_hold_until_interrupt__ Look for changed return values.".to_owned(),
                ),
            },
        )
        .await
        .expect("start AI reviewer");

    let mut reviewer_agent_id = None;
    let mut reviewer_stream = None;
    let mut suggestion = None;
    while suggestion.is_none() || reviewer_stream.is_none() {
        let env = next_env(&mut client, "AI reviewer proposal").await;
        match env.kind {
            FrameKind::NewAgent => {
                let new_agent: NewAgentPayload = env.parse_payload().expect("new AI reviewer");
                if new_agent.name == "AI Review" {
                    assert_eq!(
                        new_agent.backend_kind,
                        BackendKind::Claude,
                        "backend_kind=None should resolve through the host default backend"
                    );
                    reviewer_stream = Some(new_agent.instance_stream);
                }
            }
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event") {
                ReviewEventPayload::Snapshot { review } => panic!(
                    "review mutation emitted unexpected Snapshot for review {} while waiting for AI reviewer start",
                    review.id.0
                ),
                ReviewEventPayload::AiReviewerChanged { state }
                    if state.status == ReviewAiReviewerStatus::Running
                        && reviewer_agent_id.is_none() =>
                {
                    let agent_id = state.agent_id.expect("running AI reviewer agent id");
                    let tool_result = call_propose_review_comment_tool(
                        &fixture,
                        &agent_id,
                        &review.id,
                        location.clone(),
                    )
                    .await;
                    assert_eq!(
                        tool_result["status"], "success",
                        "unexpected tool result: {tool_result}"
                    );
                    reviewer_agent_id = Some(agent_id);
                }
                ReviewEventPayload::SuggestionUpsert {
                    suggestion: proposed,
                } if reviewer_agent_id.is_some() => {
                    suggestion = Some(proposed);
                }
                _ => {}
            },
            _ => {}
        }
    }
    let reviewer_agent_id = reviewer_agent_id.expect("reviewer agent id");
    let reviewer_stream = reviewer_stream.expect("reviewer stream");
    let suggestion = suggestion.expect("AI suggestion upsert");
    assert_eq!(suggestion.reviewer_agent_id, reviewer_agent_id);
    assert_eq!(suggestion.body, "AI found a review issue.");
    assert_eq!(suggestion.severity, ReviewSeverity::Bug);
    assert!(matches!(suggestion.state, ReviewSuggestionState::Pending));

    client
        .review_action(
            &review.id,
            ReviewActionPayload::AcceptSuggestion {
                suggestion_id: suggestion.id.clone(),
                edit: None,
            },
        )
        .await
        .expect("accept suggestion");
    match expect_review_delta(&mut client, "accepted suggestion delta").await {
        ReviewEventPayload::SuggestionUpsert {
            suggestion: accepted,
        } => {
            assert_eq!(accepted.id, suggestion.id);
            assert!(matches!(
                accepted.state,
                ReviewSuggestionState::Accepted { .. }
            ));
        }
        other => panic!("expected accepted suggestion, got {other:?}"),
    }
    match expect_review_delta(&mut client, "AI comment upsert delta").await {
        ReviewEventPayload::CommentUpsert { comment } => {
            assert_eq!(comment.body, suggestion.body);
            assert_eq!(
                comment.source,
                ReviewCommentSource::AiSuggestion {
                    suggestion_id: suggestion.id.clone(),
                    edited: false
                }
            );
        }
        other => panic!("expected AI comment upsert, got {other:?}"),
    }
    assert_no_trailing_review_snapshot(&mut client, "AcceptSuggestion deltas").await;

    let tool_result =
        call_propose_review_comment_tool(&fixture, &reviewer_agent_id, &review.id, location).await;
    assert_eq!(
        tool_result["status"], "success",
        "unexpected tool result: {tool_result}"
    );

    let rejected_suggestion =
        match expect_review_delta(&mut client, "AI rejected-suggestion upsert delta").await {
            ReviewEventPayload::SuggestionUpsert { suggestion } => suggestion,
            other => panic!("expected pending suggestion before reject, got {other:?}"),
        };
    assert_eq!(rejected_suggestion.reviewer_agent_id, reviewer_agent_id);
    assert_eq!(rejected_suggestion.body, "AI found a review issue.");
    assert!(matches!(
        rejected_suggestion.state,
        ReviewSuggestionState::Pending
    ));

    client
        .review_action(
            &review.id,
            ReviewActionPayload::RejectSuggestion {
                suggestion_id: rejected_suggestion.id.clone(),
            },
        )
        .await
        .expect("reject suggestion");
    match expect_review_delta(&mut client, "rejected suggestion delta").await {
        ReviewEventPayload::SuggestionUpsert {
            suggestion: rejected,
        } => {
            assert_eq!(rejected.id, rejected_suggestion.id);
            assert!(matches!(rejected.state, ReviewSuggestionState::Rejected));
        }
        other => panic!("expected rejected suggestion, got {other:?}"),
    }

    client
        .interrupt(&reviewer_stream)
        .await
        .expect("interrupt reviewer");
    match expect_review_delta(&mut client, "AI reviewer completed delta").await {
        ReviewEventPayload::AiReviewerChanged { state }
            if state.status == ReviewAiReviewerStatus::Completed => {}
        other => {
            panic!("unexpected event while waiting for reviewer completion: {other:?}");
        }
    }
    close_agent_and_wait(&mut client, &reviewer_stream).await;
}

#[tokio::test]
async fn submit_without_comments_emits_invalid_status() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;

    client
        .review_action(&review.id, submit_to(&agent))
        .await
        .expect("submit empty review");
    let error = expect_review_error(
        &mut client,
        "empty submit error",
        ReviewErrorCode::InvalidStatus,
    )
    .await;
    assert!(!error.fatal);
    assert_no_trailing_review_snapshot(&mut client, "Submit error").await;
}

#[tokio::test]
async fn submit_rejects_existing_agent_from_another_project() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo_a = root.path().join("review-root-a");
    let repo_b = root.path().join("review-root-b");
    fs::create_dir_all(&repo_a).expect("create repo a");
    fs::create_dir_all(&repo_b).expect("create repo b");
    seed_repo(&repo_a);
    seed_repo(&repo_b);

    let project_a = create_project(&mut client, &repo_a).await;
    let project_b = create_project(&mut client, &repo_b).await;
    let (agent_a, _session_id_a) = spawn_project_agent(&mut client, &project_a).await;
    let (agent_b, _session_id_b) = spawn_project_agent(&mut client, &project_b).await;
    let review = create_review(&mut client, &project_a, &agent_a).await;
    let _comment_id = add_comment(&mut client, &review, "Wrong project target comment.").await;

    client
        .review_action(
            &review.id,
            ReviewActionPayload::Submit {
                target: ReviewSubmitTarget::ExistingAgent {
                    agent_id: agent_b.agent_id,
                },
            },
        )
        .await
        .expect("submit to other project agent");
    let error = expect_review_error(
        &mut client,
        "wrong project target error",
        ReviewErrorCode::InvalidSubmitTarget,
    )
    .await;
    assert!(!error.fatal);
    assert_no_trailing_review_snapshot(&mut client, "wrong project Submit error").await;

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(matches!(snapshot.status, ReviewStatus::Draft));
    assert_eq!(snapshot.comments.len(), 1);
}

#[tokio::test]
async fn cancel_rules_for_draft_and_failed_submit_reviews() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;
    let draft_review = create_review(&mut client, &project, &agent).await;
    client
        .review_action(&draft_review.id, ReviewActionPayload::Cancel)
        .await
        .expect("cancel draft");
    match expect_review_delta(&mut client, "draft cancel status delta").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Cancelled { .. },
        } => {}
        other => panic!("expected cancelled status, got {other:?}"),
    }

    let retry_review = create_review(&mut client, &project, &agent).await;
    let _comment_id = add_comment(&mut client, &retry_review, "Failed submit comment.").await;
    close_agent_and_wait(&mut client, &agent.instance_stream).await;
    client
        .review_action(&retry_review.id, submit_to(&agent))
        .await
        .expect("submit offline before cancel");
    let error = expect_review_error(
        &mut client,
        "offline submit before cancel error",
        ReviewErrorCode::InvalidSubmitTarget,
    )
    .await;
    assert!(!error.fatal);
    client
        .review_action(&retry_review.id, ReviewActionPayload::Cancel)
        .await
        .expect("cancel draft after failed submit");
    match expect_review_delta(&mut client, "cancel after failed submit delta").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Cancelled { .. },
        } => {}
        other => panic!("expected cancelled status after failed submit, got {other:?}"),
    }
}

/// ReviewCreate is get-or-create for the project singleton. A caller that
/// asks again while a draft exists should be subscribed to the same review
/// instead of accumulating duplicate drafts.
#[tokio::test]
async fn second_review_create_attaches_to_existing_singleton() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent(&mut client, &project).await;

    let first = create_review(&mut client, &project, &agent).await;

    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                selection: ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
            },
        )
        .await
        .expect("send second review create");
    let second = loop {
        let env = next_env(&mut client, "second review_create bootstrap").await;
        if env.kind == FrameKind::ReviewBootstrap {
            let bootstrap: ReviewBootstrapPayload =
                env.parse_payload().expect("review bootstrap payload");
            break bootstrap.review;
        }
    };
    assert_eq!(second.id, first.id);
    assert!(matches!(second.status, ReviewStatus::Draft));

    client
        .review_action(&first.id, ReviewActionPayload::Cancel)
        .await
        .expect("cancel first draft");
    match expect_review_delta(&mut client, "first draft cancel delta").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Cancelled { .. },
        } => {}
        other => panic!("expected cancelled status, got {other:?}"),
    }

    let third = create_review(&mut client, &project, &agent).await;
    assert_ne!(
        third.id, first.id,
        "create after cancel should yield a fresh singleton id"
    );
    assert!(matches!(third.status, ReviewStatus::Draft));
}

#[tokio::test]
async fn fallback_review_create_for_existing_draft_echoes_review_list() {
    let fixture = Fixture::new().await;
    let mut owner = fixture.connect().await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut owner, &repo).await;
    let mut client = fixture.connect().await;

    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                selection: ReviewDiffSelection::Workspace {
                    scope: ProjectDiffScope::Unstaged,
                },
            },
        )
        .await
        .expect("fallback review create");

    let bootstrap = expect_project_bootstrap(&mut client, &project).await;
    let summary = bootstrap
        .review_summaries
        .iter()
        .find(|summary| summary.scope == ReviewSummaryScope::Workspace)
        .expect("active draft workspace summary");

    expect_existing_review_create_echo(&mut client, &project, &summary.id).await;
}

#[tokio::test]
async fn queued_review_bundle_clears_after_successful_enqueue() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, _session_id) = spawn_project_agent_with_prompt(
        &mut client,
        &project,
        "start review origin __mock_slow__",
        false,
    )
    .await;
    let review = create_review(&mut client, &project, &agent).await;
    let _comment_id = add_comment(&mut client, &review, "Queued delivery comment.").await;

    client
        .review_action(&review.id, submit_to(&agent))
        .await
        .expect("submit queued review");

    let mut saw_cleared = false;
    let mut saw_queued_origin = false;
    while !saw_cleared || !saw_queued_origin {
        let env = next_env(&mut client, "queued review bundle").await;
        match env.kind {
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event") {
                ReviewEventPayload::Cleared { review: cleared } => {
                    assert_eq!(cleared.id, review.id);
                    assert!(cleared.comments.is_empty());
                    saw_cleared = true;
                }
                other => {
                    panic!("unexpected review event while waiting for queued clear: {other:?}")
                }
            },
            FrameKind::QueuedMessages if env.stream == agent.instance_stream => {
                let payload: QueuedMessagesPayload =
                    env.parse_payload().expect("queued messages payload");
                saw_queued_origin = payload.messages.iter().any(|entry| {
                    entry.origin
                        == Some(MessageOrigin::Review {
                            review_id: review.id.clone(),
                        })
                });
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn rehydrate_status_variants_and_subscribe_terminal_reviews() {
    let fixture = Fixture::new().await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);
    let mut setup_client = fixture.connect().await;
    let project = create_project(&mut setup_client, &repo).await;

    let reviews_path = fixture.store_dir().join("reviews.json");
    const DRAFT_ID: &str = "550e8400-e29b-41d4-a716-446655440101";
    const SUBMITTED_ID: &str = "550e8400-e29b-41d4-a716-446655440102";
    const CONSUMED_ID: &str = "550e8400-e29b-41d4-a716-446655440103";
    const CANCELLED_ID: &str = "550e8400-e29b-41d4-a716-446655440104";
    let reviews = vec![
        sample_stored_review(
            DRAFT_ID,
            &project,
            &repo,
            ReviewStatus::Draft,
            ReviewAiReviewerStatus::Running,
        ),
        sample_stored_review(
            SUBMITTED_ID,
            &project,
            &repo,
            ReviewStatus::Submitted {
                submitted_at_ms: 10,
            },
            ReviewAiReviewerStatus::Idle,
        ),
        sample_stored_review(
            CONSUMED_ID,
            &project,
            &repo,
            ReviewStatus::Consumed {
                submitted_at_ms: 10,
                consumed_at_ms: 11,
                target_agent_id: AgentId("550e8400-e29b-41d4-a716-446655440010".to_owned()),
            },
            ReviewAiReviewerStatus::Idle,
        ),
        sample_stored_review(
            CANCELLED_ID,
            &project,
            &repo,
            ReviewStatus::Cancelled {
                cancelled_at_ms: 12,
            },
            ReviewAiReviewerStatus::Idle,
        ),
    ];
    let records = reviews
        .iter()
        .map(|review| {
            (
                review.id.0.clone(),
                serde_json::to_value(review).expect("review JSON"),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    fs::write(
        &reviews_path,
        serde_json::to_vec_pretty(&json!({ "records": records })).expect("reviews store JSON"),
    )
    .expect("write reviews store");

    let mut client = fixture.connect_fresh_host().await;
    for review in reviews {
        let snapshot = subscribe_review(&mut client, &review.id).await;
        assert!(!snapshot.diffs.is_empty());
        match review.id.0.as_str() {
            DRAFT_ID => {
                assert_eq!(snapshot.status, ReviewStatus::Draft);
                assert_eq!(snapshot.ai_reviewer.status, ReviewAiReviewerStatus::Idle);
                assert_eq!(snapshot.ai_reviewer.agent_id, None);
            }
            SUBMITTED_ID => assert!(matches!(
                snapshot.status,
                ReviewStatus::Submitted {
                    submitted_at_ms: 10
                }
            )),
            CONSUMED_ID => assert!(matches!(
                snapshot.status,
                ReviewStatus::Consumed {
                    submitted_at_ms: 10,
                    consumed_at_ms: 11,
                    ..
                }
            )),
            CANCELLED_ID => assert!(matches!(
                snapshot.status,
                ReviewStatus::Cancelled {
                    cancelled_at_ms: 12
                }
            )),
            other => panic!("unexpected review id {other}"),
        }
    }
}

#[tokio::test]
async fn legacy_project_only_drafts_do_not_surface_as_active_summaries() {
    let fixture = Fixture::new().await;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);
    let mut setup_client = fixture.connect().await;
    let project = create_project(&mut setup_client, &repo).await;

    let reviews_path = fixture.store_dir().join("reviews.json");
    let mut first = sample_stored_review(
        "550e8400-e29b-41d4-a716-446655440201",
        &project,
        &repo,
        ReviewStatus::Draft,
        ReviewAiReviewerStatus::Idle,
    );
    first.selection = ReviewDiffSelection::AllUncommitted;
    let mut second = sample_stored_review(
        "550e8400-e29b-41d4-a716-446655440202",
        &project,
        &repo,
        ReviewStatus::Draft,
        ReviewAiReviewerStatus::Idle,
    );
    second.selection = ReviewDiffSelection::AllUncommitted;
    second.updated_at_ms = 3;
    let records = [&first, &second]
        .into_iter()
        .map(|review| {
            (
                review.id.0.clone(),
                serde_json::to_value(review).expect("review JSON"),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    fs::write(
        &reviews_path,
        serde_json::to_vec_pretty(&json!({ "records": records })).expect("reviews store JSON"),
    )
    .expect("write reviews store");

    let mut client = fixture.connect_fresh_host().await;
    let bootstrap = expect_project_bootstrap(&mut client, &project).await;
    assert_eq!(bootstrap.review_summaries.len(), 1);
    let summary = &bootstrap.review_summaries[0];
    assert_eq!(summary.scope, ReviewSummaryScope::Workspace);
    assert_ne!(summary.id, first.id);
    assert_ne!(summary.id, second.id);
    assert!(matches!(summary.status, ReviewStatus::Draft));
}
