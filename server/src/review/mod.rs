pub(crate) mod actor;
pub(crate) mod bundle;
pub(crate) mod reviewer;

use std::collections::HashMap;
use std::sync::Arc;

use actor::{ConnectionId, ReviewCommand, spawn_review_actor};
use protocol::{
    AgentId, DiffContextMode, ProjectGitDiffPayload, ProjectId, Review, ReviewActionPayload,
    ReviewAiReviewerStatus, ReviewCreatePayload, ReviewDiffSelection, ReviewErrorContext,
    ReviewErrorPayload, ReviewId, ReviewStatus, ReviewSuggestionId, ReviewSummary,
    SendMessagePayload, SessionId, StreamPath,
};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::agent::now_ms;
use crate::review::actor::{ReviewAiSpawnRequest, ReviewDeliveryRequest};
use crate::store::project::ProjectStore;
use crate::store::review::ReviewStore;
use crate::stream::Stream;

#[derive(Clone)]
pub(crate) struct ReviewHandle {
    pub(crate) tx: mpsc::Sender<ReviewCommand>,
}

impl ReviewHandle {
    async fn subscribe(&self, conn: ConnectionId, stream: Stream) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ReviewCommand::Subscribe {
                conn,
                stream,
                reply,
            })
            .await
            .map_err(|_| "review actor stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review actor dropped subscribe response".to_owned())?
    }

    async fn unsubscribe(&self, conn: ConnectionId) {
        let _ = self.tx.send(ReviewCommand::Unsubscribe { conn }).await;
    }

    async fn action(&self, action: ReviewActionPayload, conn: ConnectionId) -> Result<(), String> {
        self.tx
            .send(ReviewCommand::Action { action, conn })
            .await
            .map_err(|_| "review actor stopped".to_owned())
    }

    pub(crate) async fn ai_suggestion(
        &self,
        suggestion: protocol::ReviewSuggestedComment,
    ) -> Result<Result<ReviewSuggestionId, ReviewErrorPayload>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ReviewCommand::AiSuggestion { suggestion, reply })
            .await
            .map_err(|_| "review actor stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review actor dropped AI suggestion response".to_owned())
    }

    pub(crate) async fn ai_reviewer_exited(
        &self,
        result: Result<(), String>,
    ) -> Result<(), String> {
        self.tx
            .send(ReviewCommand::AiReviewerExited { result })
            .await
            .map_err(|_| "review actor stopped".to_owned())
    }

    async fn bundle_consumed(&self, target_agent_id: AgentId, at_ms: u64) -> Result<(), String> {
        self.tx
            .send(ReviewCommand::BundleConsumed {
                target_agent_id,
                at_ms,
            })
            .await
            .map_err(|_| "review actor stopped".to_owned())
    }

    async fn internal_error(
        &self,
        message: String,
        context: ReviewErrorContext,
    ) -> Result<(), String> {
        self.tx
            .send(ReviewCommand::InternalError { message, context })
            .await
            .map_err(|_| "review actor stopped".to_owned())
    }

    async fn snapshot(&self) -> Result<Review, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ReviewCommand::Snapshot { reply })
            .await
            .map_err(|_| "review actor stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review actor dropped snapshot response".to_owned())
    }

    async fn summary(&self) -> Result<ReviewSummary, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ReviewCommand::Summary { reply })
            .await
            .map_err(|_| "review actor stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review actor dropped summary response".to_owned())
    }

    async fn submitted_bundle_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<(ReviewId, SendMessagePayload)>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ReviewCommand::SubmittedBundleForSession { session_id, reply })
            .await
            .map_err(|_| "review actor stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review actor dropped submitted bundle response".to_owned())
    }
}

#[derive(Clone)]
pub(crate) struct ReviewRegistryHandle {
    tx: mpsc::Sender<RegistryCommand>,
}

pub(crate) struct ReviewCreateRequest {
    pub review_id: ReviewId,
    pub project_id: ProjectId,
    pub origin_agent_id: AgentId,
    pub origin_session_id: SessionId,
    pub selection: ReviewDiffSelection,
    pub diffs: Vec<ProjectGitDiffPayload>,
    pub conn: ConnectionId,
    pub stream: Stream,
}

enum RegistryCommand {
    Create {
        request: ReviewCreateRequest,
        reply: oneshot::Sender<Result<ReviewId, String>>,
    },
    UnsubscribeAll {
        conn: ConnectionId,
    },
    Action {
        review_id: ReviewId,
        action: ReviewActionPayload,
        conn: ConnectionId,
        stream: Stream,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Subscribe {
        review_id: ReviewId,
        conn: ConnectionId,
        stream: Stream,
        reply: oneshot::Sender<Result<(), String>>,
    },
    AiSuggestion {
        review_id: ReviewId,
        suggestion: protocol::ReviewSuggestedComment,
        reply: oneshot::Sender<Result<Result<ReviewSuggestionId, ReviewErrorPayload>, String>>,
    },
    BundleConsumed {
        review_id: ReviewId,
        target_agent_id: AgentId,
        at_ms: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    InternalError {
        review_id: ReviewId,
        message: String,
        context: ReviewErrorContext,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Summaries {
        project_id: ProjectId,
        reply: oneshot::Sender<Result<Vec<ReviewSummary>, String>>,
    },
    SubmittedBundles {
        session_id: SessionId,
        reply: oneshot::Sender<Result<Vec<(ReviewId, SendMessagePayload)>, String>>,
    },
}

pub(crate) struct ReviewRegistry;

impl ReviewRegistry {
    pub(crate) fn spawn(
        store: ReviewStore,
        project_store: Arc<Mutex<ProjectStore>>,
        delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
        ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
        project_update_tx: mpsc::UnboundedSender<ProjectId>,
    ) -> Result<ReviewRegistryHandle, String> {
        let mut handles = HashMap::new();
        let mut rehydrated = Vec::new();
        for mut review in store.list()? {
            if reset_running_ai_reviewer(&mut review) {
                store.upsert(review.clone())?;
            }
            let handle = spawn_review_actor(
                review.clone(),
                store.clone(),
                Arc::clone(&project_store),
                delivery_tx.clone(),
                ai_spawn_tx.clone(),
                project_update_tx.clone(),
            );
            handles.insert(review.id.clone(), handle);
            rehydrated.push(review.id);
        }
        tracing::info!(review_count = rehydrated.len(), "rehydrated reviews");

        let (tx, rx) = mpsc::channel(64);
        let registry = ReviewRegistryActor {
            handles,
            store,
            project_store,
            delivery_tx,
            ai_spawn_tx,
            project_update_tx,
        };
        spawn_review_registry_task(async move { registry.run(rx).await });
        Ok(ReviewRegistryHandle { tx })
    }
}

fn spawn_review_registry_task<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(future);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-registry".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review registry runtime");
            runtime.block_on(future);
        })
        .expect("failed to spawn review registry thread");
}

impl ReviewRegistryHandle {
    pub(crate) async fn create(&self, request: ReviewCreateRequest) -> Result<ReviewId, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Create { request, reply })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped create response".to_owned())?
    }

    pub(crate) async fn unsubscribe_all(&self, conn: ConnectionId) {
        let _ = self.tx.send(RegistryCommand::UnsubscribeAll { conn }).await;
    }

    pub(crate) async fn action(
        &self,
        review_id: ReviewId,
        action: ReviewActionPayload,
        conn: ConnectionId,
        stream: Stream,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Action {
                review_id,
                action,
                conn,
                stream,
                reply,
            })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped action response".to_owned())?
    }

    pub(crate) async fn subscribe(
        &self,
        review_id: ReviewId,
        conn: ConnectionId,
        stream: Stream,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Subscribe {
                review_id,
                conn,
                stream,
                reply,
            })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped subscribe response".to_owned())?
    }

    pub(crate) async fn bundle_consumed(
        &self,
        review_id: ReviewId,
        target_agent_id: AgentId,
        at_ms: u64,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::BundleConsumed {
                review_id,
                target_agent_id,
                at_ms,
                reply,
            })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped bundle consumed response".to_owned())?
    }

    pub(crate) async fn ai_suggestion(
        &self,
        review_id: ReviewId,
        suggestion: protocol::ReviewSuggestedComment,
    ) -> Result<Result<ReviewSuggestionId, ReviewErrorPayload>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::AiSuggestion {
                review_id,
                suggestion,
                reply,
            })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped AI suggestion response".to_owned())?
    }

    pub(crate) async fn internal_error(
        &self,
        review_id: ReviewId,
        message: String,
        context: ReviewErrorContext,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::InternalError {
                review_id,
                message,
                context,
                reply,
            })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped internal error response".to_owned())?
    }

    pub(crate) async fn summaries(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ReviewSummary>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Summaries { project_id, reply })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped summaries response".to_owned())?
    }

    pub(crate) async fn submitted_bundles_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<(ReviewId, SendMessagePayload)>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::SubmittedBundles { session_id, reply })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped submitted bundles response".to_owned())?
    }
}

struct ReviewRegistryActor {
    handles: HashMap<ReviewId, ReviewHandle>,
    store: ReviewStore,
    project_store: Arc<Mutex<ProjectStore>>,
    delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
    ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
    project_update_tx: mpsc::UnboundedSender<ProjectId>,
}

impl ReviewRegistryActor {
    async fn run(mut self, mut rx: mpsc::Receiver<RegistryCommand>) {
        while let Some(command) = rx.recv().await {
            match command {
                RegistryCommand::Create { request, reply } => {
                    let result = self.create(request).await;
                    let _ = reply.send(result);
                }
                RegistryCommand::UnsubscribeAll { conn } => {
                    let handles = self.handles.values().cloned().collect::<Vec<_>>();
                    for handle in handles {
                        handle.unsubscribe(conn.clone()).await;
                    }
                }
                RegistryCommand::Action {
                    review_id,
                    action,
                    conn,
                    stream,
                    reply,
                } => {
                    let result = self.action(review_id, action, conn, stream).await;
                    let _ = reply.send(result);
                }
                RegistryCommand::Subscribe {
                    review_id,
                    conn,
                    stream,
                    reply,
                } => {
                    let result = self.subscribe(review_id, conn, stream).await;
                    let _ = reply.send(result);
                }
                RegistryCommand::AiSuggestion {
                    review_id,
                    suggestion,
                    reply,
                } => {
                    let result = match self.handles.get(&review_id) {
                        Some(handle) => handle.ai_suggestion(suggestion).await,
                        None => Err(format!("unknown review {}", review_id)),
                    };
                    let _ = reply.send(result);
                }
                RegistryCommand::BundleConsumed {
                    review_id,
                    target_agent_id,
                    at_ms,
                    reply,
                } => {
                    let result = match self.handles.get(&review_id) {
                        Some(handle) => handle.bundle_consumed(target_agent_id, at_ms).await,
                        None => Err(format!("unknown review {}", review_id)),
                    };
                    let _ = reply.send(result);
                }
                RegistryCommand::InternalError {
                    review_id,
                    message,
                    context,
                    reply,
                } => {
                    let result = match self.handles.get(&review_id) {
                        Some(handle) => handle.internal_error(message, context).await,
                        None => Err(format!("unknown review {}", review_id)),
                    };
                    let _ = reply.send(result);
                }
                RegistryCommand::Summaries { project_id, reply } => {
                    let result = self.summaries(project_id).await;
                    let _ = reply.send(result);
                }
                RegistryCommand::SubmittedBundles { session_id, reply } => {
                    let result = self.submitted_bundles(session_id).await;
                    let _ = reply.send(result);
                }
            }
        }
    }

    async fn create(&mut self, request: ReviewCreateRequest) -> Result<ReviewId, String> {
        let now = now_ms();
        let review_id = request.review_id;
        let review = Review {
            id: review_id.clone(),
            project_id: request.project_id.clone(),
            origin_agent_id: request.origin_agent_id,
            origin_session_id: request.origin_session_id,
            selection: request.selection,
            status: ReviewStatus::Draft,
            diffs: request.diffs,
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: protocol::ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.store.upsert(review.clone())?;
        let handle = spawn_review_actor(
            review,
            self.store.clone(),
            Arc::clone(&self.project_store),
            self.delivery_tx.clone(),
            self.ai_spawn_tx.clone(),
            self.project_update_tx.clone(),
        );
        handle.subscribe(request.conn, request.stream).await?;
        self.handles.insert(review_id.clone(), handle);
        let _ = self.project_update_tx.send(request.project_id);
        Ok(review_id)
    }

    async fn subscribe(
        &mut self,
        review_id: ReviewId,
        conn: ConnectionId,
        stream: Stream,
    ) -> Result<(), String> {
        let handle = self
            .handles
            .get(&review_id)
            .ok_or_else(|| format!("unknown review {}", review_id))?;
        handle.subscribe(conn, stream).await
    }

    async fn action(
        &mut self,
        review_id: ReviewId,
        action: ReviewActionPayload,
        conn: ConnectionId,
        _stream: Stream,
    ) -> Result<(), String> {
        let handle = self
            .handles
            .get(&review_id)
            .cloned()
            .ok_or_else(|| format!("unknown review {}", review_id))?;
        handle.action(action, conn).await
    }

    async fn summaries(&self, project_id: ProjectId) -> Result<Vec<ReviewSummary>, String> {
        let mut summaries = Vec::new();
        for handle in self.handles.values() {
            let summary = handle.summary().await?;
            if summary.origin_session_id.0.trim().is_empty() {
                return Err(format!("review {} has empty origin session id", summary.id));
            }
            if !matches!(summary.status, ReviewStatus::Cancelled { .. }) {
                let snapshot = handle.snapshot().await?;
                if snapshot.project_id == project_id {
                    summaries.push(summary);
                }
            }
        }
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        Ok(summaries)
    }

    async fn submitted_bundles(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<(ReviewId, SendMessagePayload)>, String> {
        let mut bundles = Vec::new();
        for handle in self.handles.values() {
            if let Some(bundle) = handle
                .submitted_bundle_for_session(session_id.clone())
                .await?
            {
                bundles.push(bundle);
            }
        }
        bundles.sort_by(|left, right| left.0.0.cmp(&right.0.0));
        Ok(bundles)
    }
}

fn reset_running_ai_reviewer(review: &mut Review) -> bool {
    if matches!(review.ai_reviewer.status, ReviewAiReviewerStatus::Running) {
        review.ai_reviewer.status = ReviewAiReviewerStatus::Idle;
        review.ai_reviewer.agent_id = None;
        review.ai_reviewer.error = None;
        review.updated_at_ms = now_ms();
        return true;
    }
    false
}

pub(crate) fn review_stream_path(review_id: &ReviewId) -> StreamPath {
    StreamPath(format!("/review/{}", review_id.0))
}

pub(crate) fn build_create_request(
    review_id: ReviewId,
    project_id: ProjectId,
    origin_session_id: SessionId,
    payload: ReviewCreatePayload,
    diffs: Vec<ProjectGitDiffPayload>,
    conn: ConnectionId,
    stream: Stream,
) -> ReviewCreateRequest {
    ReviewCreateRequest {
        review_id,
        project_id,
        origin_agent_id: payload.origin_agent_id,
        origin_session_id,
        selection: normalize_selection(payload.selection),
        diffs: normalize_diff_payloads(diffs),
        conn,
        stream,
    }
}

fn normalize_selection(selection: ReviewDiffSelection) -> ReviewDiffSelection {
    selection
}

fn normalize_diff_payloads(mut diffs: Vec<ProjectGitDiffPayload>) -> Vec<ProjectGitDiffPayload> {
    for diff in &mut diffs {
        diff.context_mode = DiffContextMode::FullFile;
    }
    diffs
}

#[cfg(test)]
mod tests {
    use protocol::{
        AgentId, ProjectDiffScope, ProjectGitDiffPayload, ProjectId, ProjectRootPath,
        ReviewAiReviewerState, ReviewDiffSelection, SessionId,
    };

    use super::*;
    use crate::review::actor::summary_for_review;

    #[test]
    fn running_ai_reviewer_resets_to_idle_on_rehydrate() {
        let mut review = Review {
            id: ReviewId("review-1".to_owned()),
            project_id: ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Uncommitted,
                path: None,
                context_mode: DiffContextMode::FullFile,
                files: Vec::new(),
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Running,
                agent_id: Some(AgentId("agent-2".to_owned())),
                error: Some("old".to_owned()),
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        assert!(reset_running_ai_reviewer(&mut review));

        assert_eq!(review.ai_reviewer.status, ReviewAiReviewerStatus::Idle);
        assert_eq!(review.ai_reviewer.agent_id, None);
        assert_eq!(review.ai_reviewer.error, None);
    }

    #[test]
    fn non_running_ai_reviewer_does_not_need_persist_on_rehydrate() {
        let mut review = Review {
            id: ReviewId("review-1".to_owned()),
            project_id: ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: Vec::new(),
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        };

        assert!(!reset_running_ai_reviewer(&mut review));
        assert_eq!(review.updated_at_ms, 1);
    }

    #[test]
    fn summary_counts_user_comments_and_pending_suggestions() {
        let review = Review {
            id: ReviewId("review-1".to_owned()),
            project_id: ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: Vec::new(),
            comments: vec![protocol::ReviewComment {
                id: protocol::ReviewCommentId("comment-1".to_owned()),
                location: protocol::ReviewLocation {
                    root: ProjectRootPath("/repo".to_owned()),
                    relative_path: "src/lib.rs".to_owned(),
                    anchor: protocol::ReviewAnchor::File,
                },
                body: "body".to_owned(),
                source: protocol::ReviewCommentSource::User,
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            suggestions: vec![protocol::ReviewSuggestedComment {
                id: protocol::ReviewSuggestionId("suggestion-1".to_owned()),
                location: protocol::ReviewLocation {
                    root: ProjectRootPath("/repo".to_owned()),
                    relative_path: "src/lib.rs".to_owned(),
                    anchor: protocol::ReviewAnchor::File,
                },
                body: "body".to_owned(),
                rationale: None,
                severity: protocol::ReviewSeverity::Info,
                state: protocol::ReviewSuggestionState::Pending,
                reviewer_agent_id: AgentId("agent-2".to_owned()),
                created_at_ms: 1,
            }],
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 2,
        };

        let summary = summary_for_review(&review);
        assert_eq!(summary.user_comment_count, 1);
        assert_eq!(summary.pending_suggestion_count, 1);
    }
}
