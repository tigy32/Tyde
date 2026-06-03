use protocol::{
    AgentErrorPayload, AgentId, ChatEvent, FrameKind, ReviewAnchorStatus, ReviewLocation,
    ReviewSeverity, ReviewSuggestedComment, ReviewSuggestionId, ReviewSuggestionState, ToolPolicy,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::now_ms;
use crate::review::ReviewHandle;
use crate::review_mcp::REVIEW_FEEDBACK_MCP_SERVER_NAME;
use crate::stream::Stream;

pub(crate) const REVIEWER_TOOL_NAME: &str = "propose_review_comment";

pub(crate) fn reviewer_tool_policy() -> ToolPolicy {
    ToolPolicy::AllowList {
        tools: vec![
            "Read".to_owned(),
            "LS".to_owned(),
            "Glob".to_owned(),
            "Grep".to_owned(),
            format!("mcp__{REVIEW_FEEDBACK_MCP_SERVER_NAME}__propose_review_comment"),
        ],
    }
}

pub(crate) struct ReviewerToolBridge;

pub(crate) struct ProposeReviewCommentArgs {
    pub(crate) location: ReviewLocation,
    pub(crate) body: String,
    pub(crate) severity: ReviewSeverity,
    pub(crate) rationale: Option<String>,
}

impl ReviewerToolBridge {
    pub(crate) fn spawn(
        reviewer_agent_id: AgentId,
        agent_handle: crate::agent::AgentHandle,
        review_handle: ReviewHandle,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge_stream_path = protocol::StreamPath(format!(
            "/agent/{}/review-bridge-{}",
            reviewer_agent_id.0,
            Uuid::new_v4()
        ));
        let stream = Stream::new(bridge_stream_path.clone(), tx);
        tracing::debug!(
            reviewer_agent_id = %reviewer_agent_id,
            bridge_stream = %bridge_stream_path,
            "attaching AI reviewer tool bridge"
        );
        tokio::spawn(async move {
            if !agent_handle.attach(stream).await {
                tracing::warn!(
                    reviewer_agent_id = %reviewer_agent_id,
                    bridge_stream = %bridge_stream_path,
                    "failed to attach AI reviewer tool bridge"
                );
                let _ = review_handle
                    .ai_reviewer_exited(Err("failed to attach reviewer tool bridge".to_owned()))
                    .await;
                return;
            }
            tracing::debug!(
                reviewer_agent_id = %reviewer_agent_id,
                bridge_stream = %bridge_stream_path,
                "attached AI reviewer tool bridge"
            );

            while let Some(envelope) = rx.recv().await {
                match envelope.kind {
                    FrameKind::AgentError => {
                        let message = match envelope.parse_payload::<AgentErrorPayload>() {
                            Ok(payload) => {
                                tracing::warn!(
                                    reviewer_agent_id = %reviewer_agent_id,
                                    bridge_stream = %bridge_stream_path,
                                    code = ?payload.code,
                                    message_len = payload.message.len(),
                                    "AI reviewer bridge received agent error"
                                );
                                payload.message
                            }
                            Err(err) => {
                                let message =
                                    format!("failed to parse reviewer agent_error: {err}");
                                tracing::warn!(
                                    reviewer_agent_id = %reviewer_agent_id,
                                    bridge_stream = %bridge_stream_path,
                                    message_len = message.len(),
                                    "AI reviewer bridge failed to parse agent error"
                                );
                                message
                            }
                        };
                        let _ = review_handle.ai_reviewer_exited(Err(message)).await;
                        return;
                    }
                    FrameKind::AgentClosed => {
                        tracing::info!(
                            reviewer_agent_id = %reviewer_agent_id,
                            bridge_stream = %bridge_stream_path,
                            "AI reviewer bridge observed agent closed"
                        );
                        let _ = review_handle.ai_reviewer_exited(Ok(())).await;
                        return;
                    }
                    FrameKind::ChatEvent => {
                        let event = match envelope.parse_payload::<ChatEvent>() {
                            Ok(event) => event,
                            Err(err) => {
                                let message = format!("failed to parse reviewer chat event: {err}");
                                tracing::warn!(
                                    reviewer_agent_id = %reviewer_agent_id,
                                    bridge_stream = %bridge_stream_path,
                                    message_len = message.len(),
                                    "AI reviewer bridge failed to parse chat event"
                                );
                                let _ = review_handle.ai_reviewer_exited(Err(message)).await;
                                return;
                            }
                        };
                        match event {
                            ChatEvent::MessageAdded(message)
                                if matches!(message.sender, protocol::MessageSender::Error) =>
                            {
                                tracing::warn!(
                                    reviewer_agent_id = %reviewer_agent_id,
                                    bridge_stream = %bridge_stream_path,
                                    message_len = message.content.len(),
                                    "AI reviewer bridge received error message"
                                );
                                let _ =
                                    review_handle.ai_reviewer_exited(Err(message.content)).await;
                                return;
                            }
                            ChatEvent::OperationCancelled(_) => {
                                tracing::info!(
                                    reviewer_agent_id = %reviewer_agent_id,
                                    bridge_stream = %bridge_stream_path,
                                    "AI reviewer bridge observed operation cancelled"
                                );
                                let _ = review_handle.ai_reviewer_exited(Ok(())).await;
                                return;
                            }
                            ChatEvent::TypingStatusChanged(false) => {
                                tracing::info!(
                                    reviewer_agent_id = %reviewer_agent_id,
                                    bridge_stream = %bridge_stream_path,
                                    "AI reviewer bridge observed idle status"
                                );
                                let _ = review_handle.ai_reviewer_exited(Ok(())).await;
                                return;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }

            tracing::info!(
                reviewer_agent_id = %reviewer_agent_id,
                bridge_stream = %bridge_stream_path,
                "AI reviewer bridge stream closed"
            );
            let _ = review_handle.ai_reviewer_exited(Ok(())).await;
        });
    }

    pub(crate) fn suggestion_from_tool_args(
        reviewer_agent_id: &AgentId,
        args: ProposeReviewCommentArgs,
    ) -> Option<ReviewSuggestedComment> {
        if args.body.trim().is_empty() {
            return None;
        }
        Some(ReviewSuggestedComment {
            id: ReviewSuggestionId(Uuid::new_v4().to_string()),
            location: args.location,
            anchor_status: ReviewAnchorStatus::Current,
            body: args.body,
            rationale: args.rationale,
            severity: args.severity,
            state: ReviewSuggestionState::Pending,
            reviewer_agent_id: reviewer_agent_id.clone(),
            created_at_ms: now_ms(),
        })
    }
}

pub(crate) fn build_reviewer_system_prompt(
    review: &protocol::Review,
    instructions: Option<String>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are the AI reviewer for a frozen Tyde code review. ");
    prompt.push_str("Do not edit files. Propose comments only by calling the ");
    prompt.push_str(REVIEWER_TOOL_NAME);
    prompt.push_str(" MCP tool. Every tool call must include the review_id shown below, a JSON location object for a changed file, body, severity, and optional rationale.\n\n");
    prompt.push_str("review_id: ");
    prompt.push_str(&review.id.0);
    prompt.push_str("\nproject_id: ");
    prompt.push_str(&review.project_id.0);
    prompt.push_str("\nDo not use project_id as location.root.\n");
    if let Some(instructions) = instructions
        && !instructions.trim().is_empty()
    {
        prompt.push_str("\nUser instructions:\n");
        prompt.push_str(instructions.trim());
        prompt.push('\n');
    }

    prompt.push_str("\nReview roots (use these exact strings as location.root):\n");
    for diff in &review.diffs {
        prompt.push_str("- ");
        prompt.push_str(&diff.root.0);
        prompt.push('\n');
    }

    prompt.push_str("\nFiles in this review (use relative_path exactly as shown):\n");
    for diff in &review.diffs {
        for file in &diff.files {
            prompt.push_str("- root: ");
            prompt.push_str(&diff.root.0);
            prompt.push_str(" relative_path: ");
            prompt.push_str(&file.relative_path);
            prompt.push('\n');
        }
    }

    prompt.push_str(
        "\nLocation JSON examples for propose_review_comment:\n\
         - Whole file: {\"root\":\"<root>\",\"relative_path\":\"<relative_path>\",\"anchor\":{\"kind\":\"file\"}}\n\
         - New-side lines: {\"root\":\"<root>\",\"relative_path\":\"<relative_path>\",\"anchor\":{\"kind\":\"line_range\",\"side\":\"new\",\"start_line\":10,\"end_line\":12}}\n\
         - Hunk: {\"root\":\"<root>\",\"relative_path\":\"<relative_path>\",\"anchor\":{\"kind\":\"hunk\",\"hunk_id\":\"<hunk_id>\",\"old_start\":1,\"old_count\":2,\"new_start\":1,\"new_count\":3}}\n",
    );
    prompt.push_str("\nThe diff is the current uncommitted git changes for the files listed above. Do not expect the diff JSON to be embedded in this prompt. Use read-only file tools to inspect the listed files. Use severity values `info`, `warn`, or `bug`. The server validates every anchor against the frozen uncommitted diff and rejects invalid locations.\n");

    prompt
}

pub(crate) fn build_reviewer_user_prompt() -> String {
    "Review the uncommitted changes listed in your system instructions and call propose_review_comment for each issue you find. If there are no issues, explain that briefly.".to_owned()
}

#[cfg(test)]
mod tests {
    use protocol::{
        DiffContextMode, ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffPayload, ProjectId,
        ProjectRootPath, Review, ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewAnchor,
        ReviewDiffSelection, ReviewDiffSide, ReviewId, ReviewStatus, SessionId,
    };

    use super::*;

    #[test]
    fn tool_args_convert_to_pending_suggestion() {
        let agent_id = AgentId("agent-1".to_owned());
        let request = ProposeReviewCommentArgs {
            location: ReviewLocation {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/lib.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: 2,
                    end_line: 2,
                },
            },
            body: "This should handle errors.".to_owned(),
            severity: ReviewSeverity::Bug,
            rationale: Some("The branch is unchecked.".to_owned()),
        };

        let suggestion =
            ReviewerToolBridge::suggestion_from_tool_args(&agent_id, request).expect("suggestion");

        assert_eq!(suggestion.reviewer_agent_id, agent_id);
        assert_eq!(suggestion.body, "This should handle errors.");
        assert_eq!(suggestion.severity, ReviewSeverity::Bug);
        assert!(matches!(suggestion.state, ReviewSuggestionState::Pending));
        assert_eq!(
            suggestion.location.root,
            ProjectRootPath("/repo".to_owned())
        );
        assert_eq!(suggestion.location.relative_path, "src/lib.rs");
        assert!(matches!(
            suggestion.location.anchor,
            ReviewAnchor::LineRange {
                side: ReviewDiffSide::New,
                start_line: 2,
                end_line: 2
            }
        ));
    }

    #[test]
    fn reviewer_prompt_uses_diff_roots_for_tool_locations() {
        let review = Review {
            id: ReviewId("review-1".to_owned()),
            project_id: ProjectId("project-uuid".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Uncommitted,
                path: None,
                context_mode: DiffContextMode::Hunks,
                files: vec![ProjectGitDiffFile {
                    relative_path: "src/lib.rs".to_owned(),
                    hunks: Vec::new(),
                }],
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        };

        let prompt = build_reviewer_system_prompt(&review, None);

        assert!(prompt.contains("Do not use project_id as location.root."));
        assert!(prompt.contains("- root: /repo relative_path: src/lib.rs"));
        assert!(prompt.contains("\"relative_path\":\"<relative_path>\""));
        assert!(prompt.contains("\"kind\":\"file\""));
        assert!(!prompt.contains("project_root: project-uuid"));
    }
}
