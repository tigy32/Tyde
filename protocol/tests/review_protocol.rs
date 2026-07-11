use std::fmt::Debug;

use protocol::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

fn round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let json = serde_json::to_string(value).expect("serialize value");
    let decoded: T = serde_json::from_str(&json).expect("deserialize value");
    assert_eq!(&decoded, value);
}

fn round_trip_json<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize value");
    let decoded: T = serde_json::from_str(&json).expect("deserialize value");
    assert_eq!(&decoded, value);
}

fn round_trip_json_without_eq<T>(value: &T)
where
    T: Serialize + DeserializeOwned,
{
    let json = serde_json::to_string(value).expect("serialize value");
    let _: T = serde_json::from_str(&json).expect("deserialize value");
}

fn review_id() -> ReviewId {
    ReviewId("review-1".to_owned())
}

fn comment_id() -> ReviewCommentId {
    ReviewCommentId("comment-1".to_owned())
}

fn suggestion_id() -> ReviewSuggestionId {
    ReviewSuggestionId("suggestion-1".to_owned())
}

fn agent_id() -> AgentId {
    AgentId("agent-1".to_owned())
}

fn session_id() -> SessionId {
    SessionId("session-1".to_owned())
}

fn project_id() -> ProjectId {
    ProjectId("project-1".to_owned())
}

fn root_path() -> ProjectRootPath {
    ProjectRootPath("/repo".to_owned())
}

fn line_location() -> ReviewLocation {
    ReviewLocation {
        root: root_path(),
        relative_path: "src/lib.rs".to_owned(),
        anchor: ReviewAnchor::LineRange {
            side: ReviewDiffSide::New,
            start_line: 10,
            end_line: 12,
        },
    }
}

fn hunk_location() -> ReviewLocation {
    ReviewLocation {
        root: root_path(),
        relative_path: "src/lib.rs".to_owned(),
        anchor: ReviewAnchor::Hunk {
            hunk_id: "src/lib.rs:1".to_owned(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 4,
        },
    }
}

fn file_location() -> ReviewLocation {
    ReviewLocation {
        root: root_path(),
        relative_path: "src/lib.rs".to_owned(),
        anchor: ReviewAnchor::File,
    }
}

fn comment() -> ReviewComment {
    ReviewComment {
        id: comment_id(),
        location: line_location(),
        anchor_status: ReviewAnchorStatus::Current,
        body: "Please adjust this.".to_owned(),
        source: ReviewCommentSource::AiSuggestion {
            suggestion_id: suggestion_id(),
            edited: true,
        },
        created_at_ms: 100,
        updated_at_ms: 200,
    }
}

fn suggestion() -> ReviewSuggestedComment {
    ReviewSuggestedComment {
        id: suggestion_id(),
        location: hunk_location(),
        anchor_status: ReviewAnchorStatus::Stale {
            reason: "old hunk disappeared".to_owned(),
        },
        body: "This looks risky.".to_owned(),
        rationale: Some("Potential regression.".to_owned()),
        severity: ReviewSeverity::Bug,
        state: ReviewSuggestionState::Accepted {
            comment_id: comment_id(),
        },
        reviewer_agent_id: agent_id(),
        created_at_ms: 150,
    }
}

fn ai_reviewer_state() -> ReviewAiReviewerState {
    ReviewAiReviewerState {
        status: ReviewAiReviewerStatus::Completed,
        agent_id: Some(agent_id()),
        error: None,
    }
}

fn diff_payload() -> ProjectGitDiffPayload {
    ProjectGitDiffPayload {
        root: root_path(),
        scope: ProjectDiffScope::Uncommitted,
        path: None,
        context_mode: DiffContextMode::FullFile,
        files: vec![ProjectGitDiffFile {
            relative_path: "src/lib.rs".to_owned(),
            is_binary: false,
            hunks: vec![ProjectGitDiffHunk {
                hunk_id: "src/lib.rs:1".to_owned(),
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 3,
                lines: vec![
                    ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Context,
                        text: "fn main() {".to_owned(),
                        old_line_number: Some(1),
                        new_line_number: Some(1),
                    },
                    ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Added,
                        text: "    println!(\"hi\");".to_owned(),
                        old_line_number: None,
                        new_line_number: Some(2),
                    },
                    ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Removed,
                        text: "}".to_owned(),
                        old_line_number: Some(2),
                        new_line_number: None,
                    },
                ],
            }],
        }],
    }
}

fn review() -> Review {
    Review {
        id: review_id(),
        project_id: project_id(),
        origin_agent_id: agent_id(),
        origin_session_id: session_id(),
        selection: ReviewDiffSelection::AllUncommitted,
        status: ReviewStatus::Draft,
        diffs: vec![diff_payload()],
        comments: vec![comment()],
        suggestions: vec![suggestion()],
        ai_reviewer: ai_reviewer_state(),
        created_at_ms: 50,
        updated_at_ms: 250,
    }
}

fn review_summary() -> ReviewSummary {
    ReviewSummary {
        id: review_id(),
        scope: ReviewSummaryScope::Workspace,
        status: ReviewStatus::Submitted {
            submitted_at_ms: 300,
        },
        origin_session_id: session_id(),
        origin_agent_id: agent_id(),
        created_at_ms: 50,
        updated_at_ms: 300,
        user_comment_count: 2,
        pending_suggestion_count: 1,
        file_comment_counts: vec![ReviewFileCommentCount {
            root: root_path(),
            relative_path: "src/lib.rs".to_owned(),
            user_comment_count: 1,
            ai_comment_count: 1,
            pending_suggestion_count: 1,
        }],
    }
}

#[test]
fn review_ids_round_trip() {
    round_trip(&review_id());
    round_trip(&comment_id());
    round_trip(&suggestion_id());
}

#[test]
fn review_data_model_round_trips() {
    round_trip(&vec![
        ReviewStatus::Draft,
        ReviewStatus::Submitted { submitted_at_ms: 1 },
        ReviewStatus::Consumed {
            submitted_at_ms: 1,
            consumed_at_ms: 2,
            target_agent_id: agent_id(),
        },
        ReviewStatus::Cancelled { cancelled_at_ms: 3 },
    ]);

    round_trip(&vec![
        ReviewDiffSelection::AllUncommitted,
        ReviewDiffSelection::Workspace {
            scope: ProjectDiffScope::Unstaged,
        },
        ReviewDiffSelection::Root {
            root: root_path(),
            scope: ProjectDiffScope::Uncommitted,
            path: Some("src/lib.rs".to_owned()),
        },
    ]);

    round_trip(&vec![file_location(), hunk_location(), line_location()]);
    round_trip(&vec![
        ReviewAnchorStatus::Current,
        ReviewAnchorStatus::Stale {
            reason: "line moved".to_owned(),
        },
    ]);
    round_trip(&ReviewCommentSource::User);
    round_trip(&ReviewCommentSource::AiSuggestion {
        suggestion_id: suggestion_id(),
        edited: false,
    });
    round_trip(&comment());
    round_trip(&suggestion());
    round_trip(&vec![
        ReviewSeverity::Info,
        ReviewSeverity::Warn,
        ReviewSeverity::Bug,
    ]);
    round_trip(&vec![
        ReviewSuggestionState::Pending,
        ReviewSuggestionState::Accepted {
            comment_id: comment_id(),
        },
        ReviewSuggestionState::Rejected,
    ]);
    round_trip(&ai_reviewer_state());
    round_trip(&vec![
        ReviewAiReviewerStatus::Idle,
        ReviewAiReviewerStatus::Running,
        ReviewAiReviewerStatus::Completed,
        ReviewAiReviewerStatus::Failed,
    ]);
    round_trip(&review());
    round_trip(&review_summary());
    round_trip(&ReviewSummaryScope::Root { root: root_path() });
}

#[test]
fn review_payload_structs_round_trip() {
    round_trip(&ReviewCreatePayload {
        selection: ReviewDiffSelection::AllUncommitted,
    });

    round_trip(&ReviewErrorPayload {
        code: ReviewErrorCode::InvalidLocation,
        message: "Bad location".to_owned(),
        fatal: false,
        context: ReviewErrorContext::AddComment,
    });

    round_trip(&ProjectEventPayload::ReviewListChanged {
        reviews: vec![review_summary()],
    });

    round_trip(&ProjectEventPayload::FilesChanged {
        files: vec![ProjectFileVersionChange {
            path: ProjectPath {
                root: root_path(),
                relative_path: "src/lib.rs".to_owned(),
            },
            version: ProjectFileVersion(30499),
        }],
    });
}

#[test]
fn review_action_tagged_union_variants_round_trip() {
    let actions = vec![
        ReviewActionPayload::AddComment {
            location: line_location(),
            body: "Please explain this.".to_owned(),
        },
        ReviewActionPayload::UpdateComment {
            comment_id: comment_id(),
            body: "Updated body.".to_owned(),
        },
        ReviewActionPayload::DeleteComment {
            comment_id: comment_id(),
        },
        ReviewActionPayload::AcceptSuggestion {
            suggestion_id: suggestion_id(),
            edit: Some("Edited accepted text.".to_owned()),
        },
        ReviewActionPayload::RejectSuggestion {
            suggestion_id: suggestion_id(),
        },
        ReviewActionPayload::StartAiReview {
            backend_kind: Some(BackendKind::Claude),
            cost_hint: Some(SpawnCostHint::Medium),
            instructions: Some("Focus on correctness.".to_owned()),
        },
        ReviewActionPayload::Submit {
            target: ReviewSubmitTarget::ExistingAgent {
                agent_id: agent_id(),
            },
        },
        ReviewActionPayload::Submit {
            target: ReviewSubmitTarget::NewAgent {
                backend_kind: BackendKind::Codex,
                cost_hint: Some(SpawnCostHint::Low),
                custom_agent_id: Some(CustomAgentId("review-fixer".to_owned())),
                name: Some("Review Fixer".to_owned()),
                instructions: Some("Fix the review comments.".to_owned()),
            },
        },
        ReviewActionPayload::ClearComments,
        ReviewActionPayload::Cancel,
    ];

    round_trip(&actions);

    let add_comment = serde_json::to_value(&actions[0]).expect("serialize action");
    assert_eq!(add_comment["kind"], json!("add_comment"));
}

#[test]
fn review_comment_anchor_status_defaults_for_legacy_json() {
    let comment: ReviewComment = serde_json::from_value(json!({
        "id": "comment-1",
        "location": line_location(),
        "body": "legacy comment",
        "source": { "kind": "user" },
        "created_at_ms": 1,
        "updated_at_ms": 2
    }))
    .expect("legacy comment without anchor status");
    assert_eq!(comment.anchor_status, ReviewAnchorStatus::Current);

    let suggestion: ReviewSuggestedComment = serde_json::from_value(json!({
        "id": "suggestion-1",
        "location": hunk_location(),
        "body": "legacy suggestion",
        "rationale": null,
        "severity": "warn",
        "state": { "state": "pending" },
        "reviewer_agent_id": "agent-1",
        "created_at_ms": 1
    }))
    .expect("legacy suggestion without anchor status");
    assert_eq!(suggestion.anchor_status, ReviewAnchorStatus::Current);
}

#[test]
fn project_git_diff_file_binary_flag_defaults_for_legacy_json() {
    let file: ProjectGitDiffFile = serde_json::from_value(json!({
        "relative_path": "src/lib.rs",
        "hunks": []
    }))
    .expect("legacy diff file without binary flag");

    assert!(!file.is_binary);
}

#[test]
fn review_summary_scope_defaults_for_legacy_json() {
    let summary: ReviewSummary = serde_json::from_value(json!({
        "id": "review-1",
        "status": { "state": "draft" },
        "origin_session_id": "session-1",
        "origin_agent_id": "agent-1",
        "created_at_ms": 1,
        "updated_at_ms": 2,
        "user_comment_count": 0,
        "pending_suggestion_count": 0
    }))
    .expect("legacy summary without scope should deserialize");

    assert_eq!(summary.scope, ReviewSummaryScope::Workspace);
    assert!(summary.file_comment_counts.is_empty());
}

#[test]
fn review_file_comment_count_round_trips_and_totals() {
    let count = ReviewFileCommentCount {
        root: root_path(),
        relative_path: "src/lib.rs".to_owned(),
        user_comment_count: 2,
        ai_comment_count: 3,
        pending_suggestion_count: 5,
    };

    round_trip(&count);
    assert_eq!(count.total_count(), 10);

    let decoded: ReviewFileCommentCount = serde_json::from_value(json!({
        "relative_path": "src/main.rs"
    }))
    .expect("missing per-file count fields should default");
    assert_eq!(decoded.root, ProjectRootPath(String::new()));
    assert_eq!(decoded.total_count(), 0);
}

#[test]
fn review_start_ai_backend_kind_is_optional() {
    let action: ReviewActionPayload = serde_json::from_value(json!({
        "kind": "start_ai_review",
        "cost_hint": null,
        "instructions": "Use the host default backend."
    }))
    .expect("start_ai_review without backend_kind should deserialize");

    assert_eq!(
        action,
        ReviewActionPayload::StartAiReview {
            backend_kind: None,
            cost_hint: None,
            instructions: Some("Use the host default backend.".to_owned()),
        }
    );
    let json = serde_json::to_value(&action).expect("serialize start_ai_review");
    assert_eq!(json["kind"], json!("start_ai_review"));
    assert!(
        json.get("backend_kind").is_none(),
        "None backend_kind should be omitted from JSON: {json:?}"
    );
}

#[test]
fn review_subscribe_payload_defaults_to_include_diffs() {
    let payload: ReviewSubscribePayload =
        serde_json::from_value(json!({})).expect("empty subscribe payload should deserialize");

    assert!(payload.include_diffs);
    assert_eq!(
        serde_json::to_value(ReviewSubscribePayload::default()).expect("serialize default"),
        json!({})
    );

    let lightweight = ReviewSubscribePayload {
        include_diffs: false,
    };
    round_trip(&lightweight);
    assert_eq!(
        serde_json::to_value(lightweight).expect("serialize lightweight subscribe"),
        json!({ "include_diffs": false })
    );
}

#[test]
fn project_git_diff_file_binary_flag_round_trips() {
    let file = ProjectGitDiffFile {
        relative_path: "assets/logo.png".to_owned(),
        is_binary: true,
        hunks: Vec::new(),
    };

    round_trip(&file);
    assert_eq!(
        serde_json::to_value(&file).expect("serialize binary diff file")["is_binary"],
        json!(true)
    );
}

#[test]
fn review_event_tagged_union_variants_round_trip() {
    let events = vec![
        ReviewEventPayload::Snapshot { review: review() },
        ReviewEventPayload::CommentUpsert { comment: comment() },
        ReviewEventPayload::CommentDelete {
            comment_id: comment_id(),
        },
        ReviewEventPayload::SuggestionUpsert {
            suggestion: suggestion(),
        },
        ReviewEventPayload::AiReviewerChanged {
            state: ai_reviewer_state(),
        },
        ReviewEventPayload::StatusChanged {
            status: ReviewStatus::Submitted {
                submitted_at_ms: 300,
            },
        },
        ReviewEventPayload::Cleared { review: review() },
        ReviewEventPayload::Error {
            error: ReviewErrorPayload {
                code: ReviewErrorCode::UnknownSuggestion,
                message: "Unknown suggestion".to_owned(),
                fatal: false,
                context: ReviewErrorContext::RejectSuggestion {
                    suggestion_id: suggestion_id(),
                },
            },
        },
    ];

    round_trip(&events);

    let snapshot = serde_json::to_value(&events[0]).expect("serialize event");
    assert_eq!(snapshot["kind"], json!("snapshot"));
}

#[test]
fn review_error_codes_and_contexts_round_trip() {
    round_trip(&vec![
        ReviewErrorCode::InvalidStatus,
        ReviewErrorCode::InvalidLocation,
        ReviewErrorCode::UnknownComment,
        ReviewErrorCode::UnknownSuggestion,
        ReviewErrorCode::InvalidSubmitTarget,
        ReviewErrorCode::OriginAgentNotRunning,
        ReviewErrorCode::AmbiguousOriginSession,
        ReviewErrorCode::ReviewerAlreadyRunning,
        ReviewErrorCode::ReviewerBackendUnsupported,
        ReviewErrorCode::GitFailed,
        ReviewErrorCode::IoFailed,
        ReviewErrorCode::Internal,
    ]);

    round_trip(&vec![
        ReviewErrorContext::AddComment,
        ReviewErrorContext::UpdateComment {
            comment_id: comment_id(),
        },
        ReviewErrorContext::DeleteComment {
            comment_id: comment_id(),
        },
        ReviewErrorContext::AcceptSuggestion {
            suggestion_id: suggestion_id(),
        },
        ReviewErrorContext::RejectSuggestion {
            suggestion_id: suggestion_id(),
        },
        ReviewErrorContext::StartAiReview,
        ReviewErrorContext::Submit,
        ReviewErrorContext::ClearComments,
        ReviewErrorContext::Cancel,
    ]);
}

#[test]
fn message_origin_round_trips_and_defaults_to_none() {
    round_trip(&MessageOrigin::User);
    round_trip(&MessageOrigin::Review {
        review_id: review_id(),
    });

    let decoded: SendMessagePayload = serde_json::from_value(json!({
        "message": "hello"
    }))
    .expect("deserialize SendMessagePayload without origin");
    assert_eq!(decoded.message, "hello");
    assert_eq!(decoded.images, None);
    assert_eq!(decoded.origin, None);
    assert_eq!(decoded.tool_response, None);

    let decoded: SendMessagePayload = serde_json::from_value(json!({
        "message": "hello",
        "origin": {
            "kind": "review",
            "review_id": "review-1"
        }
    }))
    .expect("deserialize SendMessagePayload with origin");
    assert_eq!(
        decoded.origin,
        Some(MessageOrigin::Review {
            review_id: review_id()
        })
    );

    let decoded: SendMessagePayload = serde_json::from_value(json!({
        "message": "",
        "tool_response": {
            "kind": "ExitPlanMode",
            "tool_call_id": "toolu_exit",
            "decision": "reject",
            "feedback": "Needs tests"
        }
    }))
    .expect("deserialize SendMessagePayload with tool response");
    assert_eq!(
        decoded.tool_response,
        Some(protocol::SendMessageToolResponse::ExitPlanMode {
            tool_call_id: "toolu_exit".to_string(),
            decision: protocol::ExitPlanModeDecision::Reject,
            feedback: Some("Needs tests".to_string()),
        })
    );
}

#[test]
fn new_frame_kinds_and_project_diff_scope_use_snake_case() {
    assert_eq!(FrameKind::ReviewCreate.to_string(), "review_create");
    assert_eq!(FrameKind::ReviewAction.to_string(), "review_action");
    assert_eq!(FrameKind::ReviewEvent.to_string(), "review_event");
    assert_eq!(FrameKind::ReviewSubscribe.to_string(), "review_subscribe");
    assert_eq!(FrameKind::HostBootstrap.to_string(), "host_bootstrap");
    assert_eq!(
        FrameKind::AgentActivitySummary.to_string(),
        "agent_activity_summary"
    );
    assert_eq!(FrameKind::AgentBootstrap.to_string(), "agent_bootstrap");
    assert_eq!(FrameKind::ProjectBootstrap.to_string(), "project_bootstrap");
    assert_eq!(FrameKind::ReviewBootstrap.to_string(), "review_bootstrap");
    assert_eq!(FrameKind::BrowseBootstrap.to_string(), "browse_bootstrap");
    assert_eq!(
        FrameKind::TerminalBootstrap.to_string(),
        "terminal_bootstrap"
    );
    round_trip(&ReviewSubscribePayload::default());
    assert_eq!(
        serde_json::to_value(ProjectDiffScope::Uncommitted).expect("serialize diff scope"),
        json!("uncommitted")
    );
}

#[test]
fn bootstrap_payloads_round_trip() {
    round_trip_json(&HostBootstrapPayload {
        settings: HostSettings {
            enabled_backends: vec![BackendKind::Claude],
            default_backend: Some(BackendKind::Claude),
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            code_intel: Default::default(),
            backend_config: std::collections::HashMap::new(),
            launch_profiles: Vec::new(),
        },
        mobile_access: MobileAccessStatePayload {
            broker_status: MobileBrokerStatus::Disabled,
            pairing: MobilePairingState::Idle,
            paired_devices: vec![],
        },
        backend_setup: BackendSetupPayload { backends: vec![] },
        session_schemas: vec![],
        backend_config_schemas: vec![],
        backend_config_snapshots: vec![],
        launch_profile_catalog: Default::default(),
        sessions: vec![SessionSummary {
            id: session_id(),
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: vec!["/repo".to_owned()],
            project_id: Some(project_id()),
            alias: Some("Work".to_owned()),
            user_alias: None,
            parent_id: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 3,
            token_count: Some(4),
            resumable: true,
            compacted_from_session_id: None,
            compacted_to_session_id: None,
            compacted_at_ms: None,
            compaction_summary_preview: None,
        }],
        session_list: Default::default(),
        projects: vec![Project {
            id: project_id(),
            name: "Repo".to_owned(),
            sort_order: 0,
            source: ProjectSource::Standalone {
                roots: vec![ProjectRootPath("/repo".to_owned())],
            },
        }],
        mcp_servers: vec![],
        skills: vec![],
        steering: vec![],
        custom_agents: vec![],
        team_preset_catalog: TeamPresetCatalog {
            role_presets: vec![],
            personality_traits: vec![],
            personality_presets: vec![],
            team_templates: vec![],
        },
        team_drafts: vec![],
        teams: vec![],
        team_members: vec![],
        team_member_bindings: vec![],
        agents: vec![NewAgentPayload {
            agent_id: agent_id(),
            name: "Agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: vec!["/repo".to_owned()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: Some(project_id()),
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
            instance_stream: StreamPath("/agent/agent-1/instance-1".to_owned()),
            activity_summary: Default::default(),
        }],
        task_token_usages: Vec::new(),
        workflow_summaries: vec![],
        workflow_diagnostics: vec![],
        workflow_runs: vec![],
        workflow_locations: vec![],
        agents_view_preferences: None,
    });

    round_trip_json_without_eq(&AgentBootstrapPayload {
        events: vec![AgentBootstrapEvent::ChatEvent(
            ChatEvent::TypingStatusChanged(true),
        )],
        latest_output: Default::default(),
    });
    round_trip_json(&ProjectBootstrapPayload {
        project: Project {
            id: project_id(),
            name: "Repo".to_owned(),
            sort_order: 0,
            source: ProjectSource::Standalone {
                roots: vec![ProjectRootPath("/repo".to_owned())],
            },
        },
        file_list: ProjectFileListPayload {
            incremental: false,
            roots: vec![],
        },
        git_status: ProjectGitStatusPayload { roots: vec![] },
        review_summaries: vec![review_summary()],
    });
    round_trip_json(&ReviewBootstrapPayload { review: review() });
    round_trip_json(&BrowseBootstrapPayload {
        opened: HostBrowseOpenedPayload {
            home: HostAbsPath("/home".to_owned()),
            root: HostAbsPath("/".to_owned()),
            separator: '/',
            platform: HostPlatform::Linux,
        },
        listing: BrowseBootstrapListing::Entries {
            entries: HostBrowseEntriesPayload {
                path: HostAbsPath("/home".to_owned()),
                parent: Some(HostAbsPath("/".to_owned())),
                entries: vec![],
            },
        },
    });
    round_trip_json(&TerminalBootstrapPayload {
        terminal_id: TerminalId("terminal-1".to_owned()),
        start: TerminalStartPayload {
            project_id: None,
            root: None,
            cwd: "/repo".to_owned(),
            shell: "/bin/sh".to_owned(),
            cols: 80,
            rows: 24,
            created_at_ms: 1,
        },
    });
}
