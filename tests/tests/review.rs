mod fixture;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentId, AgentStartPayload, BackendKind, ChatEvent, CommandErrorCode, CommandErrorPayload,
    DiffContextMode, Envelope, FrameKind, MessageOrigin, MessageSender, NewAgentPayload, Project,
    ProjectCreatePayload, ProjectDiffScope, ProjectGitDiffLineKind, ProjectGitDiffPayload,
    ProjectNotifyPayload, ProjectRootPath, QueuedMessagesPayload, Review, ReviewActionPayload,
    ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewAnchor, ReviewCommentId,
    ReviewCommentSource, ReviewCreatePayload, ReviewDiffSelection, ReviewDiffSide, ReviewErrorCode,
    ReviewEventPayload, ReviewId, ReviewLocation, ReviewSeverity, ReviewStatus,
    ReviewSubscribePayload, ReviewSuggestedComment, ReviewSuggestionState, SessionId,
    SessionListPayload, SpawnAgentParams, SpawnAgentPayload,
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

async fn expect_new_agent(client: &mut client::Connection, context: &str) -> NewAgentPayload {
    loop {
        let env = next_env(client, context).await;
        if env.kind == FrameKind::NewAgent {
            return env.parse_payload().expect("new agent payload");
        }
    }
}

async fn expect_agent_start(
    client: &mut client::Connection,
    stream: &protocol::StreamPath,
) -> AgentStartPayload {
    loop {
        let env = next_env(client, "agent start").await;
        if env.kind == FrameKind::AgentStart && env.stream == *stream {
            return env.parse_payload().expect("agent start payload");
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

async fn expect_review_error(
    client: &mut client::Connection,
    context: &str,
    code: ReviewErrorCode,
) -> protocol::ReviewErrorPayload {
    match expect_review_event(client, context).await {
        ReviewEventPayload::Error { error } => {
            assert_eq!(error.code, code);
            error
        }
        other => panic!("expected review error {code:?}, got {other:?}"),
    }
}

async fn subscribe_review(client: &mut client::Connection, review_id: &ReviewId) -> Review {
    client
        .review_subscribe(review_id, ReviewSubscribePayload::default())
        .await
        .expect("review subscribe");
    match expect_review_event(client, "review subscribe snapshot").await {
        ReviewEventPayload::Snapshot { review } => review,
        other => panic!("expected subscribe snapshot, got {other:?}"),
    }
}

async fn create_project(client: &mut client::Connection, root: &Path) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: "Review Project".to_owned(),
            roots: vec![root.to_string_lossy().to_string()],
        })
        .await
        .expect("project_create");
    expect_project(client, "project create").await
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
                workspace_roots: project.roots.clone(),
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
        selection: ReviewDiffSelection::AllUncommitted,
        status,
        diffs: vec![ProjectGitDiffPayload {
            root: ProjectRootPath(root.to_string_lossy().to_string()),
            scope: ProjectDiffScope::Uncommitted,
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
    origin: &NewAgentPayload,
) -> Review {
    client
        .review_create(
            &project.id,
            ReviewCreatePayload {
                origin_agent_id: origin.agent_id.clone(),
                selection: ReviewDiffSelection::AllUncommitted,
            },
        )
        .await
        .expect("review create");
    match expect_review_event(client, "review snapshot").await {
        ReviewEventPayload::Snapshot { review } => review,
        other => panic!("expected review snapshot, got {other:?}"),
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
    match expect_review_event(client, "comment upsert").await {
        ReviewEventPayload::CommentUpsert { comment } => comment.id,
        other => panic!("expected comment upsert, got {other:?}"),
    }
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
    assert_eq!(review.diffs[0].scope, ProjectDiffScope::Uncommitted);
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
    match expect_review_event(&mut client, "updated comment").await {
        ReviewEventPayload::CommentUpsert { comment } => {
            assert_eq!(comment.id, comment_id);
            assert_eq!(comment.body, "Updated comment.");
        }
        other => panic!("expected updated comment, got {other:?}"),
    }

    client
        .review_action(
            &review.id,
            ReviewActionPayload::DeleteComment {
                comment_id: comment_id.clone(),
            },
        )
        .await
        .expect("delete comment");
    match expect_review_event(&mut client, "deleted comment").await {
        ReviewEventPayload::CommentDelete { comment_id: id } => assert_eq!(id, comment_id),
        other => panic!("expected comment delete, got {other:?}"),
    }

    let _comment_id = add_comment(&mut client, &review, "Final review comment.").await;
    client
        .review_action(&review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit review");
    let mut saw_submitted = false;
    loop {
        match expect_review_event(&mut client, "submit status").await {
            ReviewEventPayload::StatusChanged {
                status: ReviewStatus::Submitted { .. },
            } => saw_submitted = true,
            ReviewEventPayload::StatusChanged {
                status:
                    ReviewStatus::Consumed {
                        target_agent_id, ..
                    },
            } => {
                assert!(saw_submitted);
                assert_eq!(target_agent_id, agent.agent_id);
                break;
            }
            other => panic!("unexpected review event while waiting for consumed: {other:?}"),
        }
    }
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
    match expect_review_event(&mut client, "comment upsert").await {
        ReviewEventPayload::CommentUpsert { .. } => {}
        other => panic!("expected comment upsert, got {other:?}"),
    }

    client
        .review_action(&review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit review");

    let mut saw_consumed_with_origin = false;
    let mut delivered_message = None;
    while !saw_consumed_with_origin || delivered_message.is_none() {
        let env = next_env(&mut client, "rendered review delivery").await;
        match env.kind {
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event") {
                ReviewEventPayload::StatusChanged {
                    status:
                        ReviewStatus::Consumed {
                            target_agent_id, ..
                        },
                } => {
                    assert_eq!(target_agent_id, agent.agent_id);
                    saw_consumed_with_origin = true;
                }
                ReviewEventPayload::StatusChanged {
                    status: ReviewStatus::Submitted { .. },
                } => {}
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
async fn submitted_review_consumes_when_origin_session_resumes() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let _comment_id = add_comment(&mut client, &review, "Offline delivery comment.").await;

    close_agent_and_wait(&mut client, &agent.instance_stream).await;

    client
        .review_action(&review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit offline review");
    match expect_review_event(&mut client, "submitted offline").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Submitted { .. },
        } => {}
        other => panic!("expected submitted status, got {other:?}"),
    }

    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Review Origin Resumed".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::Resume {
                session_id: session_id.clone(),
                prompt: None,
            },
        })
        .await
        .expect("resume origin session");
    let resumed = expect_new_agent(&mut client, "resumed agent").await;
    let _ = expect_agent_start(&mut client, &resumed.instance_stream).await;

    match expect_review_event(&mut client, "consumed after resume").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Consumed {
                target_agent_id, ..
            },
        } => {
            assert_eq!(target_agent_id, resumed.agent_id);
        }
        other => panic!("unexpected review event after resume: {other:?}"),
    }
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
    }

    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(snapshot.comments.is_empty());
}

#[tokio::test]
async fn ai_reviewer_propose_tool_accepts_suggestion() {
    let fixture = Fixture::new().await;
    let mut client = fixture.connect().await;
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
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                instructions: Some("__mock_slow__ Look for changed return values.".to_owned()),
            },
        )
        .await
        .expect("start AI reviewer");

    let mut reviewer_agent_id = None;
    let mut reviewer_stream = None;
    while reviewer_agent_id.is_none() || reviewer_stream.is_none() {
        let env = next_env(&mut client, "AI reviewer spawn").await;
        match env.kind {
            FrameKind::NewAgent => {
                let new_agent: NewAgentPayload = env.parse_payload().expect("new AI reviewer");
                if new_agent.name == "AI Review" {
                    reviewer_stream = Some(new_agent.instance_stream);
                }
            }
            FrameKind::ReviewEvent => match env.parse_payload().expect("review event") {
                ReviewEventPayload::AiReviewerChanged { state }
                    if state.status == ReviewAiReviewerStatus::Running =>
                {
                    reviewer_agent_id = state.agent_id;
                }
                _ => {}
            },
            _ => {}
        }
    }
    let reviewer_agent_id = reviewer_agent_id.expect("reviewer agent id");
    let reviewer_stream = reviewer_stream.expect("reviewer stream");

    let tool_result =
        call_propose_review_comment_tool(&fixture, &reviewer_agent_id, &review.id, location).await;
    assert_eq!(tool_result["status"], "success");

    let suggestion = match expect_review_event(&mut client, "AI suggestion upsert").await {
        ReviewEventPayload::SuggestionUpsert { suggestion } => suggestion,
        other => panic!("expected AI suggestion upsert, got {other:?}"),
    };
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
    match expect_review_event(&mut client, "accepted suggestion").await {
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
    match expect_review_event(&mut client, "AI comment upsert").await {
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

    close_agent_and_wait(&mut client, &reviewer_stream).await;
    match expect_review_event(&mut client, "AI reviewer completed").await {
        ReviewEventPayload::AiReviewerChanged { state }
            if state.status == ReviewAiReviewerStatus::Completed => {}
        other => {
            panic!("unexpected event while waiting for reviewer completion: {other:?}");
        }
    }
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
        .review_action(&review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit empty review");
    let error = expect_review_error(
        &mut client,
        "empty submit error",
        ReviewErrorCode::InvalidStatus,
    )
    .await;
    assert!(!error.fatal);
}

#[tokio::test]
async fn submit_with_multiple_live_agents_reverts_to_draft() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client;
    let root = tempfile::tempdir().expect("temp root");
    let repo = root.path().join("review-root");
    fs::create_dir_all(&repo).expect("create repo");
    seed_repo(&repo);

    let project = create_project(&mut client, &repo).await;
    let (agent, session_id) = spawn_project_agent(&mut client, &project).await;
    let review = create_review(&mut client, &project, &agent).await;
    let _comment_id = add_comment(&mut client, &review, "Ambiguous delivery comment.").await;

    client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Second Live Agent".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(project.id.clone()),
            params: SpawnAgentParams::Resume {
                session_id,
                prompt: None,
            },
        })
        .await
        .expect("spawn second live agent");
    let second = expect_new_agent(&mut client, "second live agent").await;
    let _ = expect_agent_start(&mut client, &second.instance_stream).await;

    client
        .review_action(&review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit ambiguous review");
    match expect_review_event(&mut client, "submitted before ambiguous").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Submitted { .. },
        } => {}
        other => panic!("expected submitted before ambiguous error, got {other:?}"),
    }
    let error = expect_review_error(
        &mut client,
        "ambiguous origin session error",
        ReviewErrorCode::AmbiguousOriginSession,
    )
    .await;
    assert!(!error.fatal);
    match expect_review_event(&mut client, "draft after ambiguous").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Draft,
        } => {}
        other => panic!("expected draft after ambiguous error, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_rules_for_draft_and_submitted_reviews() {
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
    match expect_review_event(&mut client, "draft cancel status").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Cancelled { .. },
        } => {}
        other => panic!("expected cancelled status, got {other:?}"),
    }

    let submitted_review = create_review(&mut client, &project, &agent).await;
    let _comment_id =
        add_comment(&mut client, &submitted_review, "Submitted cancel comment.").await;
    close_agent_and_wait(&mut client, &agent.instance_stream).await;
    client
        .review_action(&submitted_review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit offline before cancel");
    match expect_review_event(&mut client, "submitted status before cancel").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Submitted { .. },
        } => {}
        other => panic!("expected submitted status, got {other:?}"),
    }
    client
        .review_action(&submitted_review.id, ReviewActionPayload::Cancel)
        .await
        .expect("cancel submitted");
    let error = expect_review_error(
        &mut client,
        "submitted cancel error",
        ReviewErrorCode::InvalidStatus,
    )
    .await;
    assert!(!error.fatal);
}

/// At-most-one Draft per project. The frontend routes the
/// "Review changes" click to the existing Draft, but a misbehaving
/// caller (MCP, an older client) could still ask the server for a
/// second one — the server has to fail closed with a
/// `Conflict` `CommandError` so the user doesn't accumulate a stack
/// of empty drafts. Submitting or cancelling the existing Draft has
/// to make a new create succeed again.
#[tokio::test]
async fn second_review_create_is_rejected_while_a_draft_exists() {
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
                origin_agent_id: agent.agent_id.clone(),
                selection: ReviewDiffSelection::AllUncommitted,
            },
        )
        .await
        .expect("send second review create");
    let envelope = loop {
        let env = next_env(&mut client, "second review_create rejection").await;
        if env.kind == FrameKind::CommandError {
            break env;
        }
    };
    let error: CommandErrorPayload = envelope.parse_payload().expect("command error payload");
    assert_eq!(error.operation, "review_create");
    assert_eq!(error.code, CommandErrorCode::Conflict);
    assert!(!error.fatal);
    assert!(
        error.message.contains("already has a draft review"),
        "unexpected review_create error: {}",
        error.message
    );
    assert!(
        error.message.contains(&first.id.0),
        "expected error to name the existing draft id, got: {}",
        error.message
    );

    client
        .review_action(&first.id, ReviewActionPayload::Cancel)
        .await
        .expect("cancel first draft");
    match expect_review_event(&mut client, "first draft cancel").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Cancelled { .. },
        } => {}
        other => panic!("expected cancelled status, got {other:?}"),
    }

    let second = create_review(&mut client, &project, &agent).await;
    assert_ne!(
        second.id, first.id,
        "second create after cancel should yield a fresh review id"
    );
    assert!(matches!(second.status, ReviewStatus::Draft));
}

#[tokio::test]
async fn queued_review_bundle_is_consumed_only_after_backend_send() {
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
        .review_action(&review.id, ReviewActionPayload::Submit)
        .await
        .expect("submit queued review");
    match expect_review_event(&mut client, "queued submitted").await {
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Submitted { .. },
        } => {}
        other => panic!("expected submitted status, got {other:?}"),
    }

    let queued_id = loop {
        let env = next_env(&mut client, "queued review bundle").await;
        if env.kind == FrameKind::QueuedMessages && env.stream == agent.instance_stream {
            let payload: QueuedMessagesPayload =
                env.parse_payload().expect("queued messages payload");
            let entry = payload
                .messages
                .into_iter()
                .find(|entry| {
                    entry.origin
                        == Some(MessageOrigin::Review {
                            review_id: review.id.clone(),
                        })
                })
                .expect("review bundle should be queued with origin");
            break entry.id;
        }
    };

    client
        .cancel_queued_message(
            &agent.instance_stream,
            protocol::CancelQueuedMessagePayload { id: queued_id },
        )
        .await
        .expect("cancel queued review bundle");
    let snapshot = subscribe_review(&mut client, &review.id).await;
    assert!(matches!(snapshot.status, ReviewStatus::Submitted { .. }));
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
