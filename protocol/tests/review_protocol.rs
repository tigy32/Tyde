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
        status: ReviewStatus::Submitted {
            submitted_at_ms: 300,
        },
        origin_session_id: session_id(),
        origin_agent_id: agent_id(),
        created_at_ms: 50,
        updated_at_ms: 300,
        user_comment_count: 2,
        pending_suggestion_count: 1,
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
        ReviewDiffSelection::Root {
            root: root_path(),
            scope: ProjectDiffScope::Uncommitted,
            path: Some("src/lib.rs".to_owned()),
        },
    ]);

    round_trip(&vec![file_location(), hunk_location(), line_location()]);
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
}

#[test]
fn review_payload_structs_round_trip() {
    round_trip(&ReviewCreatePayload {
        origin_agent_id: agent_id(),
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
            backend_kind: BackendKind::Claude,
            cost_hint: Some(SpawnCostHint::Medium),
            instructions: Some("Focus on correctness.".to_owned()),
        },
        ReviewActionPayload::Submit,
        ReviewActionPayload::Cancel,
    ];

    round_trip(&actions);

    let add_comment = serde_json::to_value(&actions[0]).expect("serialize action");
    assert_eq!(add_comment["kind"], json!("add_comment"));
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
}

#[test]
fn new_frame_kinds_and_project_diff_scope_use_snake_case() {
    assert_eq!(FrameKind::ReviewCreate.to_string(), "review_create");
    assert_eq!(FrameKind::ReviewAction.to_string(), "review_action");
    assert_eq!(FrameKind::ReviewEvent.to_string(), "review_event");
    assert_eq!(FrameKind::ReviewSubscribe.to_string(), "review_subscribe");
    round_trip(&ReviewSubscribePayload::default());
    assert_eq!(
        serde_json::to_value(ProjectDiffScope::Uncommitted).expect("serialize diff scope"),
        json!("uncommitted")
    );
}
