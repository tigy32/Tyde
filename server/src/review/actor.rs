use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use protocol::{
    AgentId, DiffContextMode, FrameKind, MessageOrigin, Project, ProjectDiffScope,
    ProjectGitDiffFile, ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload,
    ProjectReadDiffPayload, ProjectRootPath, Review, ReviewActionPayload, ReviewAiReviewerState,
    ReviewAiReviewerStatus, ReviewAnchor, ReviewAnchorStatus, ReviewBootstrapPayload,
    ReviewComment, ReviewCommentId, ReviewCommentSource, ReviewDiffSelection, ReviewDiffSide,
    ReviewErrorCode, ReviewErrorContext, ReviewErrorPayload, ReviewEventPayload,
    ReviewFileCommentCount, ReviewId, ReviewLocation, ReviewStatus, ReviewSubmitTarget,
    ReviewSuggestedComment, ReviewSuggestionId, ReviewSuggestionState, SendMessagePayload,
    StreamPath,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::agent::now_ms;
use crate::project_stream::{is_not_git_repository_error, read_diff};
use crate::review::bundle::ReviewFeedbackBundle;
use crate::store::project::ProjectStore;
use crate::store::review::ReviewStore;
use crate::stream::Stream;

pub(crate) type ConnectionId = StreamPath;

#[derive(Debug)]
pub(crate) enum ReviewDeliveryOutcome {
    Delivered { target_agent_id: AgentId },
    Offline,
    Failed(String),
}

impl ReviewDeliveryOutcome {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Delivered { .. } => "delivered",
            Self::Offline => "offline",
            Self::Failed(_) => "failed",
        }
    }
}

pub(crate) struct ReviewDeliveryRequest {
    pub review_id: ReviewId,
    pub project_id: protocol::ProjectId,
    pub target: ReviewSubmitTarget,
    pub payload: SendMessagePayload,
    pub reply: oneshot::Sender<ReviewDeliveryOutcome>,
}

pub(crate) struct ReviewAiSpawnRequest {
    pub review_id: ReviewId,
    pub review: Review,
    pub backend_kind: Option<protocol::BackendKind>,
    pub cost_hint: Option<protocol::SpawnCostHint>,
    pub instructions: Option<String>,
    pub review_handle: crate::review::ReviewHandle,
    pub reply: oneshot::Sender<Result<AgentId, String>>,
}

pub(crate) type AiSuggestionResult = Result<ReviewSuggestionId, ReviewErrorPayload>;

pub(crate) enum ReviewCommand {
    Subscribe {
        conn: ConnectionId,
        stream: Stream,
        include_diffs: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Unsubscribe {
        conn: ConnectionId,
    },
    Action {
        action: ReviewActionPayload,
        conn: ConnectionId,
    },
    AiSuggestion {
        suggestion: ReviewSuggestedComment,
        reply: oneshot::Sender<AiSuggestionResult>,
    },
    AiReviewerExited {
        result: Result<(), String>,
    },
    BundleConsumed {
        target_agent_id: AgentId,
        at_ms: u64,
    },
    ResetForCleanWorkingTree,
    InternalError {
        message: String,
        context: ReviewErrorContext,
    },
    Snapshot {
        reply: oneshot::Sender<Review>,
    },
}

pub(crate) fn spawn_review_actor(
    review: Review,
    store: ReviewStore,
    project_store: Arc<Mutex<ProjectStore>>,
    delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
    ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
    project_update_tx: mpsc::UnboundedSender<protocol::ProjectId>,
) -> crate::review::ReviewHandle {
    let (tx, rx) = mpsc::channel(64);
    let handle = crate::review::ReviewHandle { tx };
    let actor_handle = handle.clone();
    spawn_review_task("tyde-review-actor", async move {
        let mut actor = ReviewActor {
            review,
            store,
            project_store,
            subscribers: HashMap::new(),
            delivery_tx,
            ai_spawn_tx,
            project_update_tx,
            handle: actor_handle,
        };
        actor.run(rx).await;
    });
    handle
}

fn spawn_review_task<F>(name: &'static str, future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(future);
        return;
    }

    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review actor runtime");
            runtime.block_on(future);
        })
        .expect("failed to spawn review actor thread");
}

struct ReviewActor {
    review: Review,
    store: ReviewStore,
    project_store: Arc<Mutex<ProjectStore>>,
    subscribers: HashMap<ConnectionId, ReviewSubscriber>,
    delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
    ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
    project_update_tx: mpsc::UnboundedSender<protocol::ProjectId>,
    handle: crate::review::ReviewHandle,
}

#[derive(Clone)]
struct ReviewSubscriber {
    stream: Stream,
    include_diffs: bool,
}

impl ReviewActor {
    async fn run(&mut self, mut rx: mpsc::Receiver<ReviewCommand>) {
        while let Some(command) = rx.recv().await {
            match command {
                ReviewCommand::Subscribe {
                    conn,
                    stream,
                    include_diffs,
                    reply,
                } => {
                    let result = self.subscribe(conn, stream, include_diffs).await;
                    let _ = reply.send(result);
                }
                ReviewCommand::Unsubscribe { conn } => {
                    self.subscribers.remove(&conn);
                }
                ReviewCommand::Action { action, conn } => {
                    self.handle_action(action, conn).await;
                }
                ReviewCommand::AiSuggestion { suggestion, reply } => {
                    let result = self.handle_ai_suggestion(suggestion).await;
                    let _ = reply.send(result);
                }
                ReviewCommand::AiReviewerExited { result } => {
                    self.handle_ai_reviewer_exited(result).await;
                }
                ReviewCommand::BundleConsumed {
                    target_agent_id,
                    at_ms,
                } => {
                    self.handle_bundle_consumed(target_agent_id, at_ms).await;
                }
                ReviewCommand::ResetForCleanWorkingTree => {
                    self.reset_for_clean_working_tree(None).await;
                }
                ReviewCommand::InternalError { message, context } => {
                    self.send_error(None, ReviewErrorCode::Internal, message, false, context)
                        .await;
                }
                ReviewCommand::Snapshot { reply } => {
                    let _ = reply.send(self.review.clone());
                }
            }
        }
    }

    async fn subscribe(
        &mut self,
        conn: ConnectionId,
        stream: Stream,
        include_diffs: bool,
    ) -> Result<(), String> {
        let effective_include_diffs = include_diffs
            || self
                .subscribers
                .get(&conn)
                .is_some_and(|subscriber| subscriber.include_diffs);
        tracing::debug!(
            review_id = %self.review.id,
            conn = %conn,
            stream = %stream.path(),
            subscriber_count = self.subscribers.len(),
            include_diffs,
            effective_include_diffs,
            "subscribing review stream"
        );
        if effective_include_diffs {
            self.refresh_diffs().await?;
        }
        let payload = serde_json::to_value(ReviewBootstrapPayload {
            review: review_for_subscriber(&self.review, effective_include_diffs),
        })
        .map_err(|error| format!("failed to serialize ReviewBootstrap payload: {error}"))?;
        stream
            .send_value(FrameKind::ReviewBootstrap, payload)
            .map_err(|_| "review stream closed".to_owned())?;
        self.subscribers.insert(
            conn,
            ReviewSubscriber {
                stream,
                include_diffs: effective_include_diffs,
            },
        );
        tracing::debug!(
            review_id = %self.review.id,
            subscriber_count = self.subscribers.len(),
            include_diffs,
            effective_include_diffs,
            "subscribed review stream"
        );
        Ok(())
    }

    async fn handle_action(&mut self, action: ReviewActionPayload, conn: ConnectionId) {
        let action_kind = action.kind_name();
        if !self.subscribers.contains_key(&conn) {
            tracing::warn!(
                review_id = %self.review.id,
                conn = %conn,
                action_kind,
                subscriber_count = self.subscribers.len(),
                "review action received without subscriber"
            );
        }
        match action {
            ReviewActionPayload::AddComment { location, body } => {
                self.add_comment(location, body, conn).await;
            }
            ReviewActionPayload::UpdateComment { comment_id, body } => {
                self.update_comment(comment_id, body, conn).await;
            }
            ReviewActionPayload::DeleteComment { comment_id } => {
                self.delete_comment(comment_id, conn).await;
            }
            ReviewActionPayload::AcceptSuggestion {
                suggestion_id,
                edit,
            } => {
                self.accept_suggestion(suggestion_id, edit, conn).await;
            }
            ReviewActionPayload::RejectSuggestion { suggestion_id } => {
                self.reject_suggestion(suggestion_id, conn).await;
            }
            ReviewActionPayload::StartAiReview {
                backend_kind,
                cost_hint,
                instructions,
            } => {
                self.start_ai_review(backend_kind, cost_hint, instructions, conn)
                    .await;
            }
            ReviewActionPayload::Submit { target } => {
                self.submit(target, conn).await;
            }
            ReviewActionPayload::ClearComments => {
                self.clear_comments(conn).await;
            }
            ReviewActionPayload::Cancel => {
                self.cancel(conn).await;
            }
        }
    }

    async fn add_comment(&mut self, location: ReviewLocation, body: String, conn: ConnectionId) {
        let context = ReviewErrorContext::AddComment;
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        if !self
            .refresh_diffs_or_error(Some(&conn), context.clone())
            .await
        {
            return;
        }
        if let Err(message) = validate_location(&self.review, &location) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidLocation,
                message,
                false,
                context,
            )
            .await;
            return;
        }

        let previous = self.review.clone();
        let now = now_ms();
        let comment = ReviewComment {
            id: ReviewCommentId(Uuid::new_v4().to_string()),
            location,
            anchor_status: ReviewAnchorStatus::Current,
            body,
            source: ReviewCommentSource::User,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.review.comments.push(comment.clone());
        self.review.updated_at_ms = now;
        if !self.persist_or_revert(previous, Some(&conn), context).await {
            return;
        }
        self.broadcast(ReviewEventPayload::CommentUpsert { comment })
            .await;
        self.notify_project_changed();
    }

    async fn update_comment(
        &mut self,
        comment_id: ReviewCommentId,
        body: String,
        conn: ConnectionId,
    ) {
        let context = ReviewErrorContext::UpdateComment {
            comment_id: comment_id.clone(),
        };
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        let Some(index) = self
            .review
            .comments
            .iter()
            .position(|comment| comment.id == comment_id)
        else {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::UnknownComment,
                format!("unknown review comment {}", comment_id),
                false,
                context,
            )
            .await;
            return;
        };
        // AI-derived comments are owned by their backing suggestion —
        // edits flow through `accept_suggestion(edit: Some(...))`. The
        // UI hides the inline edit affordance for them, but the server
        // has to enforce the same rule for any other caller (MCP, an
        // older client) so suggestion/comment state can't drift.
        if !matches!(
            self.review.comments[index].source,
            ReviewCommentSource::User
        ) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                format!("review comment {} is not user-authored", comment_id),
                false,
                context,
            )
            .await;
            return;
        }

        let previous = self.review.clone();
        let now = now_ms();
        self.review.comments[index].body = body;
        self.review.comments[index].updated_at_ms = now;
        self.review.updated_at_ms = now;
        let comment = self.review.comments[index].clone();
        if !self.persist_or_revert(previous, Some(&conn), context).await {
            return;
        }
        self.broadcast(ReviewEventPayload::CommentUpsert { comment })
            .await;
        self.notify_project_changed();
    }

    async fn delete_comment(&mut self, comment_id: ReviewCommentId, conn: ConnectionId) {
        let context = ReviewErrorContext::DeleteComment {
            comment_id: comment_id.clone(),
        };
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        let Some(index) = self
            .review
            .comments
            .iter()
            .position(|comment| comment.id == comment_id)
        else {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::UnknownComment,
                format!("unknown review comment {}", comment_id),
                false,
                context,
            )
            .await;
            return;
        };
        // Same rule as `update_comment`: the AI-derived comment lives
        // and dies with its `ReviewSuggestion`. Reject delete requests
        // for them so suggestion/comment state can't drift.
        if !matches!(
            self.review.comments[index].source,
            ReviewCommentSource::User
        ) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                format!("review comment {} is not user-authored", comment_id),
                false,
                context,
            )
            .await;
            return;
        }

        let previous = self.review.clone();
        let removed = self.review.comments.remove(index);
        self.review.updated_at_ms = now_ms();
        if !self.persist_or_revert(previous, Some(&conn), context).await {
            return;
        }
        self.broadcast(ReviewEventPayload::CommentDelete {
            comment_id: removed.id,
        })
        .await;
        self.notify_project_changed();
    }

    async fn accept_suggestion(
        &mut self,
        suggestion_id: ReviewSuggestionId,
        edit: Option<String>,
        conn: ConnectionId,
    ) {
        let context = ReviewErrorContext::AcceptSuggestion {
            suggestion_id: suggestion_id.clone(),
        };
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        let Some(index) = self
            .review
            .suggestions
            .iter()
            .position(|suggestion| suggestion.id == suggestion_id)
        else {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::UnknownSuggestion,
                format!("unknown review suggestion {}", suggestion_id),
                false,
                context,
            )
            .await;
            return;
        };
        if !matches!(
            self.review.suggestions[index].state,
            ReviewSuggestionState::Pending
        ) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                format!("review suggestion {} is not pending", suggestion_id),
                false,
                context,
            )
            .await;
            return;
        }
        let suggestion = self.review.suggestions[index].clone();
        if !self
            .refresh_diffs_or_error(Some(&conn), context.clone())
            .await
        {
            return;
        }
        if let Err(message) = validate_location(&self.review, &suggestion.location) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidLocation,
                message,
                false,
                context,
            )
            .await;
            return;
        }

        let previous = self.review.clone();
        let now = now_ms();
        let body = match edit {
            Some(value) => value,
            None => suggestion.body.clone(),
        };
        let edited = body != suggestion.body;
        let comment = ReviewComment {
            id: ReviewCommentId(Uuid::new_v4().to_string()),
            location: suggestion.location,
            anchor_status: ReviewAnchorStatus::Current,
            body,
            source: ReviewCommentSource::AiSuggestion {
                suggestion_id: suggestion_id.clone(),
                edited,
            },
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.review.suggestions[index].state = ReviewSuggestionState::Accepted {
            comment_id: comment.id.clone(),
        };
        self.review.comments.push(comment.clone());
        self.review.updated_at_ms = now;
        let suggestion = self.review.suggestions[index].clone();
        if !self.persist_or_revert(previous, Some(&conn), context).await {
            return;
        }
        self.broadcast(ReviewEventPayload::SuggestionUpsert { suggestion })
            .await;
        self.broadcast(ReviewEventPayload::CommentUpsert { comment })
            .await;
        self.notify_project_changed();
    }

    async fn reject_suggestion(&mut self, suggestion_id: ReviewSuggestionId, conn: ConnectionId) {
        let context = ReviewErrorContext::RejectSuggestion {
            suggestion_id: suggestion_id.clone(),
        };
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        let Some(index) = self
            .review
            .suggestions
            .iter()
            .position(|suggestion| suggestion.id == suggestion_id)
        else {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::UnknownSuggestion,
                format!("unknown review suggestion {}", suggestion_id),
                false,
                context,
            )
            .await;
            return;
        };
        // Same Pending guard as `accept_suggestion`. Without it, an
        // already-Accepted suggestion could flip to Rejected while its
        // derived `ReviewComment` stayed in the comment list — UI
        // hides the affordance, but the server invariant still has to
        // hold for any caller (MCP, future client).
        if !matches!(
            self.review.suggestions[index].state,
            ReviewSuggestionState::Pending
        ) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                format!("review suggestion {} is not pending", suggestion_id),
                false,
                context,
            )
            .await;
            return;
        }

        let previous = self.review.clone();
        self.review.suggestions[index].state = ReviewSuggestionState::Rejected;
        self.review.updated_at_ms = now_ms();
        let suggestion = self.review.suggestions[index].clone();
        if !self.persist_or_revert(previous, Some(&conn), context).await {
            return;
        }
        self.broadcast(ReviewEventPayload::SuggestionUpsert { suggestion })
            .await;
        self.notify_project_changed();
    }

    async fn start_ai_review(
        &mut self,
        backend_kind: Option<protocol::BackendKind>,
        cost_hint: Option<protocol::SpawnCostHint>,
        instructions: Option<String>,
        conn: ConnectionId,
    ) {
        let context = ReviewErrorContext::StartAiReview;
        let instructions_len = instructions.as_ref().map_or(0, String::len);
        tracing::info!(
            review_id = %self.review.id,
            conn = %conn,
            backend_kind = ?backend_kind,
            cost_hint = ?cost_hint,
            instructions_len,
            current_status = self.review.ai_reviewer.status.status_label(),
            diff_count = self.review.diffs.len(),
            comment_count = self.review.comments.len(),
            suggestion_count = self.review.suggestions.len(),
            "starting AI review"
        );
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        if matches!(
            self.review.ai_reviewer.status,
            ReviewAiReviewerStatus::Running
        ) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::ReviewerAlreadyRunning,
                "AI reviewer is already running".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }
        if !self
            .refresh_diffs_or_error(Some(&conn), context.clone())
            .await
        {
            return;
        }

        let (reply, response) = oneshot::channel();
        let request = ReviewAiSpawnRequest {
            review_id: self.review.id.clone(),
            review: self.review.clone(),
            backend_kind,
            cost_hint,
            instructions,
            review_handle: self.handle.clone(),
            reply,
        };
        let spawn_wait_started = Instant::now();
        tracing::debug!(
            review_id = %self.review.id,
            backend_kind = ?backend_kind,
            "requesting AI reviewer spawn"
        );
        if self.ai_spawn_tx.send(request).await.is_err() {
            tracing::warn!(
                review_id = %self.review.id,
                backend_kind = ?backend_kind,
                "AI reviewer spawn channel unavailable"
            );
            self.send_error(
                Some(&conn),
                ReviewErrorCode::ReviewerBackendUnsupported,
                "AI reviewer spawn path is unavailable".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }

        match response.await {
            Ok(Ok(agent_id)) => {
                tracing::info!(
                    review_id = %self.review.id,
                    reviewer_agent_id = %agent_id,
                    elapsed_ms = spawn_wait_started.elapsed().as_millis() as u64,
                    "AI reviewer spawn succeeded"
                );
                let previous = self.review.clone();
                self.review.ai_reviewer = ReviewAiReviewerState {
                    status: ReviewAiReviewerStatus::Running,
                    agent_id: Some(agent_id),
                    error: None,
                };
                self.review.updated_at_ms = now_ms();
                if !self.persist_or_revert(previous, Some(&conn), context).await {
                    return;
                }
                self.broadcast(ReviewEventPayload::AiReviewerChanged {
                    state: self.review.ai_reviewer.clone(),
                })
                .await;
            }
            Ok(Err(message)) => {
                tracing::warn!(
                    review_id = %self.review.id,
                    backend_kind = ?backend_kind,
                    elapsed_ms = spawn_wait_started.elapsed().as_millis() as u64,
                    error_len = message.len(),
                    "AI reviewer spawn failed"
                );
                let previous = self.review.clone();
                self.review.ai_reviewer = ReviewAiReviewerState {
                    status: ReviewAiReviewerStatus::Failed,
                    agent_id: None,
                    error: Some(message.clone()),
                };
                self.review.updated_at_ms = now_ms();
                if self
                    .persist_or_revert(previous, Some(&conn), context.clone())
                    .await
                {
                    self.broadcast(ReviewEventPayload::AiReviewerChanged {
                        state: self.review.ai_reviewer.clone(),
                    })
                    .await;
                }
                self.send_error(
                    Some(&conn),
                    ReviewErrorCode::ReviewerBackendUnsupported,
                    message,
                    false,
                    context,
                )
                .await;
            }
            Err(_) => {
                tracing::warn!(
                    review_id = %self.review.id,
                    backend_kind = ?backend_kind,
                    elapsed_ms = spawn_wait_started.elapsed().as_millis() as u64,
                    "AI reviewer spawn response dropped"
                );
                self.send_error(
                    Some(&conn),
                    ReviewErrorCode::ReviewerBackendUnsupported,
                    "AI reviewer spawn task stopped".to_owned(),
                    false,
                    context,
                )
                .await;
            }
        }
    }

    async fn submit(&mut self, target: ReviewSubmitTarget, conn: ConnectionId) {
        let context = ReviewErrorContext::Submit;
        tracing::info!(
            review_id = %self.review.id,
            conn = %conn,
            status = self.review.status.status_label(),
            comment_count = self.review.comments.len(),
            suggestion_count = self.review.suggestions.len(),
            ai_reviewer_status = self.review.ai_reviewer.status.status_label(),
            target = ?target,
            "submit review requested"
        );
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        if self.review.comments.is_empty() {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                "cannot submit a review with no comments".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }
        if matches!(
            self.review.ai_reviewer.status,
            ReviewAiReviewerStatus::Running
        ) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                "cannot submit while the AI reviewer is running".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }
        if !self
            .refresh_diffs_or_error(Some(&conn), context.clone())
            .await
        {
            tracing::warn!(
                review_id = %self.review.id,
                conn = %conn,
                "submit review stopped after diff refresh failure"
            );
            return;
        }
        if self.review.comments.is_empty() {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                "cannot submit because the review was reset for a clean working tree".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }
        let diff_stats = diff_stats(&self.review.diffs);
        tracing::debug!(
            review_id = %self.review.id,
            diff_count = diff_stats.diff_count,
            file_count = diff_stats.file_count,
            hunk_count = diff_stats.hunk_count,
            line_count = diff_stats.line_count,
            "submit review diff refresh complete"
        );
        if let Err(message) = self.validate_comment_locations() {
            tracing::warn!(
                review_id = %self.review.id,
                conn = %conn,
                error_len = message.len(),
                "submit review stopped after location validation failure"
            );
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidLocation,
                message,
                false,
                context,
            )
            .await;
            return;
        }
        let payload = match self.submit_payload() {
            Ok(payload) => {
                tracing::debug!(
                    review_id = %self.review.id,
                    message_len = payload.message.len(),
                    images_count = payload.images.as_ref().map_or(0, Vec::len),
                    "built submitted review delivery payload"
                );
                payload
            }
            Err(message) => {
                tracing::warn!(
                    review_id = %self.review.id,
                    error_len = message.len(),
                    "failed to build submitted review delivery payload"
                );
                self.send_error(
                    Some(&conn),
                    ReviewErrorCode::Internal,
                    message,
                    false,
                    context,
                )
                .await;
                return;
            }
        };

        let (reply, response) = oneshot::channel();
        let request = ReviewDeliveryRequest {
            review_id: self.review.id.clone(),
            project_id: self.review.project_id.clone(),
            target,
            payload,
            reply,
        };
        tracing::info!(
            review_id = %self.review.id,
            project_id = %self.review.project_id,
            "requesting review delivery"
        );
        if self.delivery_tx.send(request).await.is_err() {
            tracing::warn!(
                review_id = %self.review.id,
                project_id = %self.review.project_id,
                "review delivery channel unavailable"
            );
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidSubmitTarget,
                "review delivery task stopped".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }

        match response.await {
            Ok(ReviewDeliveryOutcome::Delivered { target_agent_id }) => {
                tracing::info!(
                    review_id = %self.review.id,
                    target_agent_id = %target_agent_id,
                    outcome = "delivered",
                    "review delivery completed"
                );
                self.clear_review_state(false, Some(&conn), context).await;
            }
            Ok(ReviewDeliveryOutcome::Offline) => {
                tracing::info!(
                    review_id = %self.review.id,
                    project_id = %self.review.project_id,
                    outcome = "offline",
                    "review delivery target is offline"
                );
                self.send_error(
                    Some(&conn),
                    ReviewErrorCode::InvalidSubmitTarget,
                    "review submit target is not running".to_owned(),
                    false,
                    context,
                )
                .await;
            }
            Ok(ReviewDeliveryOutcome::Failed(message)) => {
                tracing::warn!(
                    review_id = %self.review.id,
                    project_id = %self.review.project_id,
                    outcome = "failed",
                    error_len = message.len(),
                    "review delivery failed"
                );
                self.send_error(
                    Some(&conn),
                    ReviewErrorCode::InvalidSubmitTarget,
                    message,
                    false,
                    context,
                )
                .await;
            }
            Err(_) => {
                tracing::warn!(
                    review_id = %self.review.id,
                    project_id = %self.review.project_id,
                    "review delivery response dropped"
                );
                self.send_error(
                    Some(&conn),
                    ReviewErrorCode::InvalidSubmitTarget,
                    "review delivery task dropped response".to_owned(),
                    false,
                    context,
                )
                .await;
            }
        }
    }

    async fn clear_comments(&mut self, conn: ConnectionId) {
        let context = ReviewErrorContext::ClearComments;
        if !self.ensure_draft(&conn, context.clone()).await {
            return;
        }
        self.clear_review_state(false, Some(&conn), context).await;
    }

    async fn cancel(&mut self, conn: ConnectionId) {
        let context = ReviewErrorContext::Cancel;
        if !matches!(self.review.status, ReviewStatus::Draft) {
            self.send_error(
                Some(&conn),
                ReviewErrorCode::InvalidStatus,
                "only draft reviews can be cancelled".to_owned(),
                false,
                context,
            )
            .await;
            return;
        }
        let previous = self.review.clone();
        let cancelled_at_ms = now_ms();
        self.review.status = ReviewStatus::Cancelled { cancelled_at_ms };
        self.review.updated_at_ms = cancelled_at_ms;
        if !self.persist_or_revert(previous, Some(&conn), context).await {
            return;
        }
        self.broadcast(ReviewEventPayload::StatusChanged {
            status: self.review.status.clone(),
        })
        .await;
        self.notify_project_changed();
    }

    async fn handle_ai_suggestion(
        &mut self,
        mut suggestion: ReviewSuggestedComment,
    ) -> AiSuggestionResult {
        let context = ReviewErrorContext::StartAiReview;
        tracing::debug!(
            review_id = %self.review.id,
            suggestion_id = %suggestion.id,
            reviewer_agent_id = %suggestion.reviewer_agent_id,
            severity = suggestion.severity.label(),
            body_len = suggestion.body.len(),
            rationale_len = suggestion.rationale.as_ref().map_or(0, String::len),
            status = self.review.status.status_label(),
            ai_reviewer_status = self.review.ai_reviewer.status.status_label(),
            "received AI reviewer suggestion"
        );
        if !matches!(self.review.status, ReviewStatus::Draft) {
            let error = review_error(
                ReviewErrorCode::InvalidStatus,
                "AI reviewer can only add suggestions to a draft review".to_owned(),
                false,
                context,
            );
            self.broadcast(ReviewEventPayload::Error {
                error: error.clone(),
            })
            .await;
            return Err(error);
        }
        if !matches!(
            self.review.ai_reviewer.status,
            ReviewAiReviewerStatus::Running
        ) || self.review.ai_reviewer.agent_id.as_ref() != Some(&suggestion.reviewer_agent_id)
        {
            let error = review_error(
                ReviewErrorCode::InvalidStatus,
                format!(
                    "agent {} is not the running reviewer for review {}",
                    suggestion.reviewer_agent_id, self.review.id
                ),
                false,
                context,
            );
            self.broadcast(ReviewEventPayload::Error {
                error: error.clone(),
            })
            .await;
            return Err(error);
        }
        if suggestion.body.trim().is_empty() {
            let error = review_error(
                ReviewErrorCode::InvalidStatus,
                "AI reviewer suggestion body must not be empty".to_owned(),
                false,
                context,
            );
            self.broadcast(ReviewEventPayload::Error {
                error: error.clone(),
            })
            .await;
            return Err(error);
        }
        if let Err(message) = self.refresh_diffs().await {
            let error = review_error(ReviewErrorCode::GitFailed, message, false, context);
            self.broadcast(ReviewEventPayload::Error {
                error: error.clone(),
            })
            .await;
            return Err(error);
        }
        if let Err(message) = validate_location(&self.review, &suggestion.location) {
            let error = review_error(ReviewErrorCode::InvalidLocation, message, false, context);
            self.broadcast(ReviewEventPayload::Error {
                error: error.clone(),
            })
            .await;
            return Err(error);
        }
        suggestion.state = ReviewSuggestionState::Pending;
        suggestion.anchor_status = ReviewAnchorStatus::Current;
        let suggestion_id = suggestion.id.clone();
        let previous = self.review.clone();
        match self
            .review
            .suggestions
            .iter()
            .position(|existing| existing.id == suggestion.id)
        {
            Some(index) => self.review.suggestions[index] = suggestion.clone(),
            None => self.review.suggestions.push(suggestion.clone()),
        }
        self.review.updated_at_ms = now_ms();
        if !self
            .persist_or_revert(previous, None, ReviewErrorContext::StartAiReview)
            .await
        {
            return Err(review_error(
                ReviewErrorCode::IoFailed,
                "failed to persist AI reviewer suggestion".to_owned(),
                false,
                ReviewErrorContext::StartAiReview,
            ));
        }
        self.broadcast(ReviewEventPayload::SuggestionUpsert { suggestion })
            .await;
        self.notify_project_changed();
        tracing::info!(
            review_id = %self.review.id,
            suggestion_id = %suggestion_id,
            pending_suggestion_count = self
                .review
                .suggestions
                .iter()
                .filter(|suggestion| {
                    matches!(suggestion.state, ReviewSuggestionState::Pending)
                })
                .count(),
            "accepted AI reviewer suggestion"
        );
        Ok(suggestion_id)
    }

    async fn handle_ai_reviewer_exited(&mut self, result: Result<(), String>) {
        tracing::info!(
            review_id = %self.review.id,
            current_status = self.review.ai_reviewer.status.status_label(),
            result = if result.is_ok() { "ok" } else { "error" },
            error_len = result.as_ref().err().map_or(0, String::len),
            "AI reviewer exited"
        );
        if !matches!(
            self.review.ai_reviewer.status,
            ReviewAiReviewerStatus::Running
        ) {
            return;
        }
        let previous = self.review.clone();
        match result {
            Ok(()) => {
                self.review.ai_reviewer.status = ReviewAiReviewerStatus::Completed;
                self.review.ai_reviewer.error = None;
            }
            Err(message) => {
                self.review.ai_reviewer.status = ReviewAiReviewerStatus::Failed;
                self.review.ai_reviewer.error = Some(message);
            }
        }
        self.review.updated_at_ms = now_ms();
        if !self
            .persist_or_revert(previous, None, ReviewErrorContext::StartAiReview)
            .await
        {
            return;
        }
        self.broadcast(ReviewEventPayload::AiReviewerChanged {
            state: self.review.ai_reviewer.clone(),
        })
        .await;
        tracing::info!(
            review_id = %self.review.id,
            status = self.review.ai_reviewer.status.status_label(),
            reviewer_agent_id = self
                .review
                .ai_reviewer
                .agent_id
                .as_ref()
                .map(|id| id.0.as_str())
                .unwrap_or("<none>"),
            error_len = self.review.ai_reviewer.error.as_ref().map_or(0, String::len),
            "updated AI reviewer status"
        );
        self.notify_project_changed();
    }

    async fn handle_bundle_consumed(&mut self, target_agent_id: AgentId, at_ms: u64) {
        tracing::info!(
            review_id = %self.review.id,
            target_agent_id = %target_agent_id,
            at_ms,
            status = self.review.status.status_label(),
            "review bundle consumed notification received"
        );
        let ReviewStatus::Submitted { submitted_at_ms } = self.review.status.clone() else {
            tracing::debug!(
                review_id = %self.review.id,
                target_agent_id = %target_agent_id,
                status = self.review.status.status_label(),
                "ignoring consumed notification for inline review delivery"
            );
            return;
        };
        let previous = self.review.clone();
        self.review.status = ReviewStatus::Consumed {
            submitted_at_ms,
            consumed_at_ms: at_ms,
            target_agent_id: target_agent_id.clone(),
        };
        self.review.updated_at_ms = at_ms;
        if !self
            .persist_or_revert(previous, None, ReviewErrorContext::Submit)
            .await
        {
            return;
        }
        self.broadcast(ReviewEventPayload::StatusChanged {
            status: self.review.status.clone(),
        })
        .await;
        tracing::info!(
            review_id = %self.review.id,
            target_agent_id = %target_agent_id,
            status = self.review.status.status_label(),
            "marked review bundle consumed"
        );
        self.notify_project_changed();
    }

    fn submit_payload(&self) -> Result<SendMessagePayload, String> {
        let bundle = ReviewFeedbackBundle::from_review(&self.review)?;
        let message = bundle.render_markdown()?;
        Ok(SendMessagePayload {
            message,
            images: None,
            origin: Some(MessageOrigin::Review {
                review_id: self.review.id.clone(),
            }),
            tool_response: None,
        })
    }

    async fn reset_for_clean_working_tree(&mut self, conn: Option<&ConnectionId>) {
        if !review_has_user_state(&self.review) && self.review.diffs.is_empty() {
            return;
        }
        tracing::info!(
            review_id = %self.review.id,
            project_id = %self.review.project_id,
            "resetting review for clean working tree"
        );
        self.clear_review_state(true, conn, ReviewErrorContext::ClearComments)
            .await;
    }

    async fn clear_review_state(
        &mut self,
        clear_diffs: bool,
        conn: Option<&ConnectionId>,
        context: ReviewErrorContext,
    ) -> bool {
        let previous = self.review.clone();
        self.review.status = ReviewStatus::Draft;
        self.review.comments.clear();
        self.review.suggestions.clear();
        self.review.ai_reviewer = ReviewAiReviewerState {
            status: ReviewAiReviewerStatus::Idle,
            agent_id: None,
            error: None,
        };
        if clear_diffs {
            self.review.diffs.clear();
        }
        self.review.updated_at_ms = now_ms();
        if !self
            .persist_or_revert(previous, conn, context.clone())
            .await
        {
            return false;
        }
        self.broadcast(ReviewEventPayload::Cleared {
            review: self.review.clone(),
        })
        .await;
        self.notify_project_changed();
        true
    }

    async fn ensure_draft(&mut self, conn: &ConnectionId, context: ReviewErrorContext) -> bool {
        if matches!(self.review.status, ReviewStatus::Draft) {
            return true;
        }
        self.send_error(
            Some(conn),
            ReviewErrorCode::InvalidStatus,
            "review is not in draft status".to_owned(),
            false,
            context,
        )
        .await;
        false
    }

    async fn refresh_diffs_or_error(
        &mut self,
        conn: Option<&ConnectionId>,
        context: ReviewErrorContext,
    ) -> bool {
        match self.refresh_diffs().await {
            Ok(()) => true,
            Err(message) => {
                self.send_error(conn, ReviewErrorCode::GitFailed, message, false, context)
                    .await;
                false
            }
        }
    }

    async fn refresh_diffs(&mut self) -> Result<(), String> {
        let started = Instant::now();
        tracing::debug!(
            review_id = %self.review.id,
            project_id = %self.review.project_id,
            selection_kind = self.review.selection.kind_name(),
            "refreshing review diffs"
        );
        let project = {
            let store = self.project_store.lock().await;
            match store.get(&self.review.project_id) {
                Some(project) => project,
                None => {
                    let message = format!("unknown project {}", self.review.project_id);
                    tracing::warn!(
                        review_id = %self.review.id,
                        project_id = %self.review.project_id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error_len = message.len(),
                        "failed to refresh review diffs"
                    );
                    return Err(message);
                }
            }
        };
        match read_review_diffs(&project, &self.review.selection) {
            Ok(diffs) => {
                let stats = diff_stats(&diffs);
                let previous = self.review.clone();
                self.review.diffs = diffs;
                if diff_is_clean(&self.review.diffs) {
                    self.reset_for_clean_working_tree(None).await;
                    return Ok(());
                }
                let anchor_updates = refresh_anchor_statuses(&mut self.review);
                if anchor_updates.has_changes() {
                    self.review.updated_at_ms = now_ms();
                    self.store.upsert(self.review.clone())?;
                    for comment in anchor_updates.comments {
                        self.broadcast(ReviewEventPayload::CommentUpsert { comment })
                            .await;
                    }
                    for suggestion in anchor_updates.suggestions {
                        self.broadcast(ReviewEventPayload::SuggestionUpsert { suggestion })
                            .await;
                    }
                    self.notify_project_changed();
                } else {
                    self.review.updated_at_ms = previous.updated_at_ms;
                }
                tracing::info!(
                    review_id = %self.review.id,
                    project_id = %self.review.project_id,
                    selection_kind = self.review.selection.kind_name(),
                    diff_count = stats.diff_count,
                    file_count = stats.file_count,
                    hunk_count = stats.hunk_count,
                    line_count = stats.line_count,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "refreshed review diffs"
                );
            }
            Err(message) => {
                tracing::warn!(
                    review_id = %self.review.id,
                    project_id = %self.review.project_id,
                    selection_kind = self.review.selection.kind_name(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error_len = message.len(),
                    "failed to refresh review diffs"
                );
                return Err(message);
            }
        }
        Ok(())
    }

    fn validate_comment_locations(&self) -> Result<(), String> {
        for comment in &self.review.comments {
            if let ReviewAnchorStatus::Stale { reason } = &comment.anchor_status {
                return Err(format!(
                    "review comment {} has a stale anchor: {}",
                    comment.id, reason
                ));
            }
            validate_location(&self.review, &comment.location)?;
        }
        Ok(())
    }

    async fn persist_or_revert(
        &mut self,
        previous: Review,
        conn: Option<&ConnectionId>,
        context: ReviewErrorContext,
    ) -> bool {
        match self.store.upsert(self.review.clone()) {
            Ok(()) => true,
            Err(message) => {
                self.review = previous;
                self.send_error(conn, ReviewErrorCode::IoFailed, message, false, context)
                    .await;
                false
            }
        }
    }

    async fn send_error(
        &mut self,
        conn: Option<&ConnectionId>,
        code: ReviewErrorCode,
        message: String,
        fatal: bool,
        context: ReviewErrorContext,
    ) {
        tracing::debug!(
            review_id = %self.review.id,
            target = if conn.is_some() { "subscriber" } else { "broadcast" },
            code = code.code_name(),
            context = context.kind_name(),
            fatal,
            message_len = message.len(),
            "sending review error event"
        );
        let payload = ReviewEventPayload::Error {
            error: ReviewErrorPayload {
                code,
                message,
                fatal,
                context,
            },
        };
        match conn {
            Some(conn) => self.send_to_conn(conn, payload).await,
            None => self.broadcast(payload).await,
        }
    }

    async fn broadcast(&mut self, payload: ReviewEventPayload) {
        let mut dead = Vec::new();
        let event_kind = payload.kind_name();
        let subscribers = self
            .subscribers
            .iter()
            .map(|(conn, subscriber)| {
                (
                    conn.clone(),
                    subscriber.stream.clone(),
                    subscriber.include_diffs,
                )
            })
            .collect::<Vec<_>>();
        tracing::debug!(
            review_id = %self.review.id,
            event_kind,
            subscriber_count = subscribers.len(),
            "broadcasting review event"
        );
        for (conn, stream, include_diffs) in subscribers {
            let payload = event_for_subscriber(&payload, include_diffs);
            if self.send_to_stream(&stream, payload).await.is_err() {
                tracing::warn!(
                    review_id = %self.review.id,
                    conn = %conn,
                    stream = %stream.path(),
                    event_kind,
                    "review event broadcast subscriber closed"
                );
                dead.push(conn);
            }
        }
        for conn in dead {
            self.subscribers.remove(&conn);
        }
    }

    async fn send_to_conn(&mut self, conn: &ConnectionId, payload: ReviewEventPayload) {
        let event_kind = payload.kind_name();
        let Some(subscriber) = self.subscribers.get(conn).cloned() else {
            tracing::warn!(
                review_id = %self.review.id,
                conn = %conn,
                event_kind,
                subscriber_count = self.subscribers.len(),
                "targeted review event missing subscriber"
            );
            return;
        };
        let payload = event_for_subscriber(&payload, subscriber.include_diffs);
        if self
            .send_to_stream(&subscriber.stream, payload)
            .await
            .is_err()
        {
            tracing::warn!(
                review_id = %self.review.id,
                conn = %conn,
                stream = %subscriber.stream.path(),
                event_kind,
                "targeted review event subscriber closed"
            );
            self.subscribers.remove(conn);
        }
    }

    async fn send_to_stream(
        &self,
        stream: &Stream,
        payload: ReviewEventPayload,
    ) -> Result<(), String> {
        let event_kind = payload.kind_name();
        let payload = serde_json::to_value(&payload).map_err(|err| {
            tracing::warn!(
                review_id = %self.review.id,
                stream = %stream.path(),
                event_kind,
                error_len = err.to_string().len(),
                "failed to serialize review event"
            );
            format!("failed to serialize review event: {err}")
        })?;
        stream
            .send_value(FrameKind::ReviewEvent, payload)
            .map_err(|_| {
                tracing::warn!(
                    review_id = %self.review.id,
                    stream = %stream.path(),
                    event_kind,
                    "review stream closed while sending event"
                );
                "review stream closed".to_owned()
            })
    }

    fn notify_project_changed(&self) {
        let _ = self.project_update_tx.send(self.review.project_id.clone());
    }
}

fn read_review_diffs(
    project: &Project,
    selection: &ReviewDiffSelection,
) -> Result<Vec<ProjectGitDiffPayload>, String> {
    match selection {
        ReviewDiffSelection::AllUncommitted => {
            let mut diffs = Vec::new();
            for root in &project.roots {
                let payload = ProjectReadDiffPayload {
                    root: ProjectRootPath(root.clone()),
                    scope: ProjectDiffScope::Unstaged,
                    path: None,
                    context_mode: DiffContextMode::FullFile,
                };
                match read_diff(project, payload) {
                    Ok(diff) => diffs.push(diff),
                    Err(error) if is_not_git_repository_error(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(diffs)
        }
        ReviewDiffSelection::Root { root, path, .. } => {
            let payload = ProjectReadDiffPayload {
                root: root.clone(),
                scope: ProjectDiffScope::Unstaged,
                path: path.clone(),
                context_mode: DiffContextMode::FullFile,
            };
            match read_diff(project, payload) {
                Ok(diff) => Ok(vec![diff]),
                Err(error) if is_not_git_repository_error(&error) => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DiffStats {
    diff_count: usize,
    file_count: usize,
    hunk_count: usize,
    line_count: usize,
}

fn diff_stats(diffs: &[ProjectGitDiffPayload]) -> DiffStats {
    let file_count = diffs.iter().map(|diff| diff.files.len()).sum();
    let hunk_count = diffs
        .iter()
        .flat_map(|diff| diff.files.iter())
        .map(|file| file.hunks.len())
        .sum();
    let line_count = diffs
        .iter()
        .flat_map(|diff| diff.files.iter())
        .flat_map(|file| file.hunks.iter())
        .map(|hunk| hunk.lines.len())
        .sum();
    DiffStats {
        diff_count: diffs.len(),
        file_count,
        hunk_count,
        line_count,
    }
}

#[derive(Default)]
struct AnchorStatusUpdates {
    comments: Vec<ReviewComment>,
    suggestions: Vec<ReviewSuggestedComment>,
}

impl AnchorStatusUpdates {
    fn has_changes(&self) -> bool {
        !self.comments.is_empty() || !self.suggestions.is_empty()
    }
}

fn refresh_anchor_statuses(review: &mut Review) -> AnchorStatusUpdates {
    let comment_statuses = review
        .comments
        .iter()
        .map(|comment| anchor_status_for_location(review, &comment.location))
        .collect::<Vec<_>>();
    let suggestion_statuses = review
        .suggestions
        .iter()
        .map(|suggestion| anchor_status_for_location(review, &suggestion.location))
        .collect::<Vec<_>>();

    let mut updates = AnchorStatusUpdates::default();
    for (comment, status) in review.comments.iter_mut().zip(comment_statuses) {
        if comment.anchor_status != status {
            comment.anchor_status = status;
            updates.comments.push(comment.clone());
        }
    }
    for (suggestion, status) in review.suggestions.iter_mut().zip(suggestion_statuses) {
        if suggestion.anchor_status != status {
            suggestion.anchor_status = status;
            updates.suggestions.push(suggestion.clone());
        }
    }
    updates
}

fn anchor_status_for_location(review: &Review, location: &ReviewLocation) -> ReviewAnchorStatus {
    match validate_location(review, location) {
        Ok(()) => ReviewAnchorStatus::Current,
        Err(reason) => ReviewAnchorStatus::Stale { reason },
    }
}

fn diff_is_clean(diffs: &[ProjectGitDiffPayload]) -> bool {
    diffs.iter().all(|diff| diff.files.is_empty())
}

fn review_has_user_state(review: &Review) -> bool {
    !review.comments.is_empty()
        || !review.suggestions.is_empty()
        || !matches!(review.ai_reviewer.status, ReviewAiReviewerStatus::Idle)
        || review.ai_reviewer.agent_id.is_some()
        || review.ai_reviewer.error.is_some()
}

fn review_error(
    code: ReviewErrorCode,
    message: String,
    fatal: bool,
    context: ReviewErrorContext,
) -> ReviewErrorPayload {
    ReviewErrorPayload {
        code,
        message,
        fatal,
        context,
    }
}

pub(crate) fn review_for_subscriber(review: &Review, include_diffs: bool) -> Review {
    if include_diffs {
        return review.clone();
    }
    let mut review = review.clone();
    review.diffs.clear();
    review
}

pub(crate) fn event_for_subscriber(
    payload: &ReviewEventPayload,
    include_diffs: bool,
) -> ReviewEventPayload {
    match payload {
        ReviewEventPayload::Snapshot { review } => ReviewEventPayload::Snapshot {
            review: review_for_subscriber(review, include_diffs),
        },
        ReviewEventPayload::Cleared { review } => ReviewEventPayload::Cleared {
            review: review_for_subscriber(review, include_diffs),
        },
        _ => payload.clone(),
    }
}

pub(crate) fn summary_for_review(review: &Review) -> protocol::ReviewSummary {
    let user_comment_count = review.comments.len() as u32;
    let pending_suggestion_count = review
        .suggestions
        .iter()
        .filter(|suggestion| matches!(suggestion.state, ReviewSuggestionState::Pending))
        .count() as u32;
    let file_comment_counts = review_file_comment_counts(review);
    protocol::ReviewSummary {
        id: review.id.clone(),
        root: review_summary_root(review).unwrap_or_default(),
        status: review.status.clone(),
        origin_session_id: review.origin_session_id.clone(),
        origin_agent_id: review.origin_agent_id.clone(),
        created_at_ms: review.created_at_ms,
        updated_at_ms: review.updated_at_ms,
        user_comment_count,
        pending_suggestion_count,
        file_comment_counts,
    }
}

pub(crate) fn review_file_comment_counts(review: &Review) -> Vec<ReviewFileCommentCount> {
    let mut counts = BTreeMap::<String, ReviewFileCommentCount>::new();
    for comment in &review.comments {
        let count = counts
            .entry(comment.location.relative_path.clone())
            .or_insert_with(|| ReviewFileCommentCount {
                relative_path: comment.location.relative_path.clone(),
                ..ReviewFileCommentCount::default()
            });
        match &comment.source {
            ReviewCommentSource::User => {
                count.user_comment_count = count.user_comment_count.saturating_add(1);
            }
            ReviewCommentSource::AiSuggestion { .. } => {
                count.ai_comment_count = count.ai_comment_count.saturating_add(1);
            }
        }
    }
    for suggestion in &review.suggestions {
        if !matches!(suggestion.state, ReviewSuggestionState::Pending) {
            continue;
        }
        let count = counts
            .entry(suggestion.location.relative_path.clone())
            .or_insert_with(|| ReviewFileCommentCount {
                relative_path: suggestion.location.relative_path.clone(),
                ..ReviewFileCommentCount::default()
            });
        count.pending_suggestion_count = count.pending_suggestion_count.saturating_add(1);
    }
    counts.into_values().collect()
}

pub(crate) fn review_summary_root(review: &Review) -> Option<ProjectRootPath> {
    match &review.selection {
        ReviewDiffSelection::Root { root, .. } => Some(root.clone()),
        ReviewDiffSelection::AllUncommitted => match review.diffs.as_slice() {
            [diff] => Some(diff.root.clone()),
            _ => None,
        },
    }
}

pub(crate) fn validate_location(review: &Review, location: &ReviewLocation) -> Result<(), String> {
    let file = find_file(review, location).ok_or_else(|| {
        format!(
            "review {} has no file {} in root {}",
            review.id, location.relative_path, location.root
        )
    })?;

    match &location.anchor {
        ReviewAnchor::File => Ok(()),
        ReviewAnchor::Hunk {
            hunk_id,
            old_start,
            old_count,
            new_start,
            new_count,
        } => {
            let Some(hunk) = file.hunks.iter().find(|hunk| hunk.hunk_id == *hunk_id) else {
                return Err(format!(
                    "unknown hunk {} for {}",
                    hunk_id, location.relative_path
                ));
            };
            if hunk.old_start == *old_start
                && hunk.old_count == *old_count
                && hunk.new_start == *new_start
                && hunk.new_count == *new_count
            {
                Ok(())
            } else {
                Err(format!(
                    "hunk {} coordinates do not match the frozen diff",
                    hunk_id
                ))
            }
        }
        ReviewAnchor::LineRange {
            side,
            start_line,
            end_line,
        } => {
            if start_line > end_line {
                return Err(format!(
                    "line range start {} must be <= end {}",
                    start_line, end_line
                ));
            }
            for line_number in *start_line..=*end_line {
                if !file_has_line(file, *side, line_number) {
                    return Err(format!(
                        "line {} on {:?} side is not present in the frozen diff for {}",
                        line_number, side, location.relative_path
                    ));
                }
            }
            Ok(())
        }
    }
}

fn find_file<'a>(review: &'a Review, location: &ReviewLocation) -> Option<&'a ProjectGitDiffFile> {
    review
        .diffs
        .iter()
        .find(|diff| diff.root == location.root)
        .and_then(|diff| {
            diff.files
                .iter()
                .find(|file| file.relative_path == location.relative_path)
        })
}

fn file_has_line(file: &ProjectGitDiffFile, side: ReviewDiffSide, line_number: u32) -> bool {
    file.hunks.iter().any(|hunk| {
        hunk.lines
            .iter()
            .any(|line| line_is_valid_anchor(line, side, line_number))
    })
}

fn line_is_valid_anchor(line: &ProjectGitDiffLine, side: ReviewDiffSide, line_number: u32) -> bool {
    match side {
        ReviewDiffSide::Old => {
            line.old_line_number == Some(line_number)
                && matches!(
                    line.kind,
                    ProjectGitDiffLineKind::Removed | ProjectGitDiffLineKind::Context
                )
        }
        ReviewDiffSide::New => {
            line.new_line_number == Some(line_number)
                && matches!(
                    line.kind,
                    ProjectGitDiffLineKind::Added | ProjectGitDiffLineKind::Context
                )
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{
        DiffContextMode, ProjectDiffScope, ProjectGitDiffHunk, ProjectGitDiffPayload,
        ProjectRootPath, ReviewAiReviewerState, ReviewDiffSelection, ReviewSeverity,
    };

    use super::*;

    fn sample_review() -> Review {
        Review {
            id: ReviewId("review-1".to_owned()),
            project_id: protocol::ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: protocol::SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Uncommitted,
                path: None,
                context_mode: DiffContextMode::FullFile,
                files: vec![ProjectGitDiffFile {
                    relative_path: "src/lib.rs".to_owned(),
                    is_binary: false,
                    hunks: vec![ProjectGitDiffHunk {
                        hunk_id: "src/lib.rs::0".to_owned(),
                        old_start: 1,
                        old_count: 2,
                        new_start: 1,
                        new_count: 2,
                        lines: vec![
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Context,
                                text: "same".to_owned(),
                                old_line_number: Some(1),
                                new_line_number: Some(1),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Removed,
                                text: "old".to_owned(),
                                old_line_number: Some(2),
                                new_line_number: None,
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "new".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(2),
                            },
                        ],
                    }],
                }],
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn validate_location_accepts_correct_line_sides() {
        let review = sample_review();
        validate_location(
            &review,
            &ReviewLocation {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/lib.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: 2,
                    end_line: 2,
                },
            },
        )
        .expect("new added line valid");
        validate_location(
            &review,
            &ReviewLocation {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/lib.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::Old,
                    start_line: 2,
                    end_line: 2,
                },
            },
        )
        .expect("old removed line valid");
    }

    #[test]
    fn validate_location_rejects_wrong_side_and_out_of_range() {
        let review = sample_review();
        let wrong_side = ReviewLocation {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/lib.rs".to_owned(),
            anchor: ReviewAnchor::LineRange {
                side: ReviewDiffSide::Old,
                start_line: 3,
                end_line: 3,
            },
        };
        assert!(validate_location(&review, &wrong_side).is_err());

        let reversed = ReviewLocation {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/lib.rs".to_owned(),
            anchor: ReviewAnchor::LineRange {
                side: ReviewDiffSide::New,
                start_line: 3,
                end_line: 2,
            },
        };
        assert!(validate_location(&review, &reversed).is_err());
    }

    #[test]
    fn validate_location_accepts_file_anchor_for_binary_file() {
        let mut review = sample_review();
        review.diffs[0].files = vec![ProjectGitDiffFile {
            relative_path: "assets/logo.png".to_owned(),
            is_binary: true,
            hunks: Vec::new(),
        }];

        validate_location(
            &review,
            &ReviewLocation {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "assets/logo.png".to_owned(),
                anchor: ReviewAnchor::File,
            },
        )
        .expect("binary file-level anchor valid");

        let line_anchor = ReviewLocation {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "assets/logo.png".to_owned(),
            anchor: ReviewAnchor::LineRange {
                side: ReviewDiffSide::New,
                start_line: 1,
                end_line: 1,
            },
        };
        assert!(validate_location(&review, &line_anchor).is_err());
    }

    #[test]
    fn diff_is_clean_treats_binary_file_as_dirty() {
        let diffs = vec![ProjectGitDiffPayload {
            root: ProjectRootPath("/repo".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: vec![ProjectGitDiffFile {
                relative_path: "assets/logo.png".to_owned(),
                is_binary: true,
                hunks: Vec::new(),
            }],
        }];

        assert!(!diff_is_clean(&diffs));
    }

    #[test]
    fn diff_is_clean_treats_metadata_only_file_as_dirty() {
        let diffs = vec![ProjectGitDiffPayload {
            root: ProjectRootPath("/repo".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: vec![ProjectGitDiffFile {
                relative_path: "scripts/run.sh".to_owned(),
                is_binary: false,
                hunks: Vec::new(),
            }],
        }];

        assert!(!diff_is_clean(&diffs));
    }

    #[test]
    fn diff_is_clean_accepts_only_empty_file_lists() {
        let diffs = vec![ProjectGitDiffPayload {
            root: ProjectRootPath("/repo".to_owned()),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: Vec::new(),
        }];

        assert!(diff_is_clean(&diffs));
    }

    #[test]
    fn refresh_anchor_statuses_marks_comments_stale_without_reanchoring() {
        let mut review = sample_review();
        let original_location = ReviewLocation {
            root: ProjectRootPath("/repo".to_owned()),
            relative_path: "src/lib.rs".to_owned(),
            anchor: ReviewAnchor::LineRange {
                side: ReviewDiffSide::New,
                start_line: 2,
                end_line: 2,
            },
        };
        review.comments.push(ReviewComment {
            id: ReviewCommentId("comment-1".to_owned()),
            location: original_location.clone(),
            anchor_status: ReviewAnchorStatus::Current,
            body: "body".to_owned(),
            source: ReviewCommentSource::User,
            created_at_ms: 1,
            updated_at_ms: 1,
        });
        review.diffs[0].files[0].hunks[0]
            .lines
            .retain(|line| line.kind != ProjectGitDiffLineKind::Added);

        let updates = refresh_anchor_statuses(&mut review);

        assert_eq!(updates.comments.len(), 1);
        assert_eq!(review.comments[0].location, original_location);
        assert!(matches!(
            review.comments[0].anchor_status,
            ReviewAnchorStatus::Stale { .. }
        ));
    }

    #[test]
    fn ai_suggestion_state_is_pending_before_insert() {
        let mut suggestion = ReviewSuggestedComment {
            id: ReviewSuggestionId("suggestion-1".to_owned()),
            location: ReviewLocation {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: "src/lib.rs".to_owned(),
                anchor: ReviewAnchor::File,
            },
            anchor_status: ReviewAnchorStatus::Current,
            body: "body".to_owned(),
            rationale: None,
            severity: ReviewSeverity::Warn,
            state: ReviewSuggestionState::Rejected,
            reviewer_agent_id: AgentId("agent-2".to_owned()),
            created_at_ms: 1,
        };
        suggestion.state = ReviewSuggestionState::Pending;
        assert!(matches!(suggestion.state, ReviewSuggestionState::Pending));
    }
}
