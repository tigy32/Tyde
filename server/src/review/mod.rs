pub(crate) mod bundle;
pub(crate) mod reviewer;
pub(crate) mod state;

use std::cmp::Ordering;
use std::collections::HashMap;

use protocol::{
    AgentId, DiffContextMode, Project, ProjectDiffScope, ProjectGitDiffPayload, ProjectId,
    ProjectRootPath, Review, ReviewActionPayload, ReviewAiReviewerStatus, ReviewCreatePayload,
    ReviewDiffSelection, ReviewErrorContext, ReviewErrorPayload, ReviewId, ReviewStatus,
    ReviewSuggestionId, ReviewSummary, SessionId, StreamPath,
};
use state::{ConnectionId, ReviewCommand, ReviewState};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use crate::agent::now_ms;
use crate::project_stream::ProjectStreamHandle;
use crate::review::state::{ReviewAiSpawnRequest, ReviewDeliveryRequest};
use crate::store::review::ReviewStore;
use crate::stream::Stream;

#[derive(Clone)]
pub(crate) struct ReviewHandle {
    pub(crate) tx: mpsc::UnboundedSender<(ReviewId, ReviewCommand)>,
    pub(crate) review_id: ReviewId,
    pub(crate) changes: watch::Receiver<()>,
}

impl ReviewHandle {
    pub(crate) fn send(&self, command: ReviewCommand) -> Result<(), String> {
        self.tx
            .send((self.review_id.clone(), command))
            .map_err(|_| "project review owner stopped".to_owned())
    }

    pub(crate) async fn request_review(
        &self,
        caller: AgentId,
        scope: protocol::ReviewAiScope,
        mode: Option<protocol::ReviewMode>,
    ) -> Result<Review, String> {
        let (reply, response) = oneshot::channel();
        self.send(ReviewCommand::AgentRequest {
            mode,
            caller,
            scope,
            reply,
        })
        .map_err(|_| "review stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review request dropped".to_owned())?
    }

    pub(crate) async fn edit_finding(
        &self,
        suggestion_id: ReviewSuggestionId,
        body: String,
    ) -> Result<Review, String> {
        let (reply, response) = oneshot::channel();
        self.send(ReviewCommand::EditFinding {
            suggestion_id,
            body,
            reply,
        })
        .map_err(|_| "review stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review comment edit dropped".to_owned())?
    }

    pub(crate) async fn disposition(
        &self,
        suggestion_id: ReviewSuggestionId,
        reason: String,
    ) -> Result<Review, String> {
        let (reply, response) = oneshot::channel();
        self.send(ReviewCommand::Disposition {
            suggestion_id,
            reason,
            reply,
        })
        .map_err(|_| "review stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review disposition dropped".to_owned())?
    }

    pub(crate) async fn ai_reviewer_exited(
        &self,
        agent_id: AgentId,
        result: Result<(), String>,
    ) -> Result<(), String> {
        self.send(ReviewCommand::AiReviewerExited { agent_id, result })
            .map_err(|_| "project review owner stopped".to_owned())
    }

    pub(crate) async fn snapshot(&self) -> Result<Review, String> {
        let (reply, response) = oneshot::channel();
        self.send(ReviewCommand::Snapshot { reply })
            .map_err(|_| "project review owner stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project review owner dropped snapshot response".to_owned())
    }
}

#[derive(Clone)]
pub(crate) struct ReviewRegistryHandle {
    tx: mpsc::UnboundedSender<RegistryCommand>,
    runtime: ReviewRuntime,
}

#[derive(Clone)]
struct ReviewRuntime {
    store: ReviewStore,
    delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
    ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
}

pub(crate) struct ReviewCreateRequest {
    pub review_id: ReviewId,
    pub project_id: ProjectId,
    pub selection: ReviewDiffSelection,
    pub diffs: Vec<ProjectGitDiffPayload>,
}

pub(crate) enum RegistryCommand {
    RegisterProject {
        project_id: ProjectId,
        handle: ProjectStreamHandle,
    },
    RegisterReview {
        review_id: ReviewId,
        project_id: ProjectId,
    },
    Handle {
        review_id: ReviewId,
        reply: oneshot::Sender<Result<ReviewHandle, String>>,
    },
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
        reply: oneshot::Sender<Result<(), String>>,
    },
    Subscribe {
        review_id: ReviewId,
        conn: ConnectionId,
        stream: Stream,
        include_diffs: bool,
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
    DeleteForProject {
        project_id: ProjectId,
        reply: oneshot::Sender<Result<Vec<ReviewId>, String>>,
    },
}
pub(crate) struct ReviewRegistry;

impl ReviewRegistry {
    pub(crate) fn spawn(
        store: ReviewStore,
        routes: Vec<(ReviewId, ProjectId)>,
        delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
        ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
    ) -> ReviewRegistryHandle {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runtime = ReviewRuntime {
            store: store.clone(),
            delivery_tx,
            ai_spawn_tx,
        };
        spawn_review_registry_task(async move {
            let mut projects = HashMap::<ProjectId, ProjectStreamHandle>::new();
            let mut owners = routes.into_iter().collect::<HashMap<_, _>>();
            while let Some(command) = rx.recv().await {
                match command {
                    RegistryCommand::RegisterProject { project_id, handle } => {
                        projects.insert(project_id, handle);
                    }
                    RegistryCommand::RegisterReview {
                        review_id,
                        project_id,
                    } => {
                        owners.insert(review_id, project_id);
                    }
                    RegistryCommand::UnsubscribeAll { conn } => {
                        for project in projects.values() {
                            if let Err(error) =
                                project.route_review_registry(RegistryCommand::UnsubscribeAll {
                                    conn: conn.clone(),
                                })
                            {
                                tracing::warn!(%error, "failed to unsubscribe project reviews");
                            }
                        }
                    }
                    RegistryCommand::DeleteForProject { project_id, reply } => {
                        projects.remove(&project_id);
                        owners.retain(|_, owner| owner != &project_id);
                        let store = store.clone();
                        tokio::spawn(async move {
                            let _ = reply.send(store.delete_for_project(&project_id).await);
                        });
                    }
                    command => {
                        let project_id = match &command {
                            RegistryCommand::Create { request, .. } => Some(&request.project_id),
                            RegistryCommand::Summaries { project_id, .. } => Some(project_id),
                            RegistryCommand::Handle { review_id, .. }
                            | RegistryCommand::Action { review_id, .. }
                            | RegistryCommand::Subscribe { review_id, .. }
                            | RegistryCommand::AiSuggestion { review_id, .. }
                            | RegistryCommand::BundleConsumed { review_id, .. }
                            | RegistryCommand::InternalError { review_id, .. } => {
                                owners.get(review_id)
                            }
                            RegistryCommand::RegisterProject { .. }
                            | RegistryCommand::RegisterReview { .. }
                            | RegistryCommand::UnsubscribeAll { .. }
                            | RegistryCommand::DeleteForProject { .. } => None,
                        };
                        match project_id.and_then(|id| projects.get(id)) {
                            Some(project) => {
                                if let Err(error) = project.route_review_registry(command) {
                                    tracing::warn!(%error, "failed to route review operation");
                                }
                            }
                            None => reject_registry_command(
                                command,
                                "Unknown project or review".to_owned(),
                            ),
                        }
                    }
                }
            }
        });
        ReviewRegistryHandle { tx, runtime }
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
        .name("tyde-project-review-directory".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build project review directory runtime");
            runtime.block_on(future);
        })
        .expect("failed to spawn project review directory thread");
}

fn reject_registry_command(command: RegistryCommand, error: String) {
    match command {
        RegistryCommand::Handle { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        RegistryCommand::Create { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        RegistryCommand::Action { reply, .. }
        | RegistryCommand::Subscribe { reply, .. }
        | RegistryCommand::BundleConsumed { reply, .. }
        | RegistryCommand::InternalError { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        RegistryCommand::AiSuggestion { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        RegistryCommand::Summaries { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        RegistryCommand::DeleteForProject { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        RegistryCommand::RegisterProject { .. }
        | RegistryCommand::RegisterReview { .. }
        | RegistryCommand::UnsubscribeAll { .. } => {
            tracing::warn!(%error, "invalid project review directory operation");
        }
    }
}

impl ReviewRegistryHandle {
    pub(crate) fn register_project(
        &self,
        project_id: ProjectId,
        handle: ProjectStreamHandle,
    ) -> Result<(), String> {
        self.tx
            .send(RegistryCommand::RegisterProject { project_id, handle })
            .map_err(|_| "project review directory stopped".to_owned())
    }

    fn register_review(&self, review_id: ReviewId, project_id: ProjectId) -> Result<(), String> {
        self.tx
            .send(RegistryCommand::RegisterReview {
                review_id,
                project_id,
            })
            .map_err(|_| "project review directory stopped".to_owned())
    }

    pub(crate) async fn handle(&self, review_id: ReviewId) -> Result<ReviewHandle, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Handle { review_id, reply })
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped response".to_owned())?
    }

    pub(crate) async fn create(&self, request: ReviewCreateRequest) -> Result<ReviewId, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Create { request, reply })
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped create response".to_owned())?
    }

    pub(crate) async fn unsubscribe_all(&self, conn: ConnectionId) {
        let _ = self.tx.send(RegistryCommand::UnsubscribeAll { conn });
    }

    pub(crate) async fn action(
        &self,
        review_id: ReviewId,
        action: ReviewActionPayload,
        conn: ConnectionId,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Action {
                review_id,
                action,
                conn,
                reply,
            })
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
        include_diffs: bool,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::Subscribe {
                review_id,
                conn,
                stream,
                include_diffs,
                reply,
            })
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
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped summaries response".to_owned())?
    }

    /// Deletes every persisted review referencing `project_id` (used when a
    /// project or workbench is deleted) and returns the removed review ids.
    pub(crate) async fn delete_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ReviewId>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::DeleteForProject { project_id, reply })
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped delete-for-project response".to_owned())?
    }
}
pub(crate) struct ProjectReviews {
    reviews: HashMap<ReviewId, ReviewState>,
    changed: bool,
    registry: ReviewRegistryHandle,
    tx: mpsc::UnboundedSender<(ReviewId, ReviewCommand)>,
}

impl ProjectReviews {
    pub(crate) async fn load(
        project: &Project,
        registry: ReviewRegistryHandle,
        tx: mpsc::UnboundedSender<(ReviewId, ReviewCommand)>,
    ) -> Result<Self, String> {
        let mut owner = Self {
            reviews: HashMap::new(),
            changed: false,
            registry,
            tx,
        };
        for mut review in owner
            .registry
            .runtime
            .store
            .list_for_project(&project.id)
            .await?
        {
            let mut changed = reset_running_ai_reviewer(&mut review);
            changed |= normalize_rehydrated_workspace_review(&mut review);
            if changed {
                owner.registry.runtime.store.upsert(review.clone()).await?;
            }
            owner.insert(review)?;
        }
        if !owner.reviews.values().any(|state| {
            matches!(state.review.status, ReviewStatus::Draft)
                && active_review_selection(&state.review.selection)
        }) {
            owner
                .create(ReviewCreateRequest {
                    review_id: ReviewId(Uuid::new_v4().to_string()),
                    project_id: project.id.clone(),
                    selection: active_workspace_selection(),
                    diffs: Vec::new(),
                })
                .await?;
        }
        Ok(owner)
    }

    fn insert(&mut self, review: Review) -> Result<(), String> {
        self.registry
            .register_review(review.id.clone(), review.project_id.clone())?;
        let runtime = &self.registry.runtime;
        let state = ReviewState::new(
            review,
            runtime.store.clone(),
            runtime.delivery_tx.clone(),
            runtime.ai_spawn_tx.clone(),
            self.tx.clone(),
        );
        self.reviews.insert(state.review.id.clone(), state);
        self.changed = true;
        Ok(())
    }

    pub(crate) fn summaries(&self) -> Vec<ReviewSummary> {
        let mut drafts = self
            .reviews
            .values()
            .map(|state| &state.review)
            .filter(|review| {
                matches!(review.status, ReviewStatus::Draft)
                    && active_review_selection(&review.selection)
            })
            .collect::<Vec<_>>();
        drafts.sort_by(|left, right| active_review_sort(left, right));
        drafts
            .first()
            .map(|review| vec![state::summary_for_review(review)])
            .unwrap_or_default()
    }

    pub(crate) fn take_changed(&mut self) -> bool {
        self.reviews
            .values_mut()
            .fold(std::mem::take(&mut self.changed), |changed, state| {
                state.take_changed() || changed
            })
    }

    pub(crate) async fn handle_command(
        &mut self,
        project: &Project,
        review_id: ReviewId,
        command: ReviewCommand,
    ) {
        let Some(state) = self.reviews.get_mut(&review_id) else {
            tracing::warn!(%review_id, "review operation has no state in its project owner");
            return;
        };
        state.handle_command(project, command).await;
        while let Some(command) = state.next_deferred() {
            state.handle_command(project, command).await;
        }
    }

    pub(crate) async fn reset_clean_roots(&mut self, project: &Project, roots: &[ProjectRootPath]) {
        if !project.root_paths().iter().all(|root| roots.contains(root)) {
            return;
        }
        let active = self
            .reviews
            .values()
            .filter(|state| {
                matches!(state.review.status, ReviewStatus::Draft)
                    && active_review_selection(&state.review.selection)
            })
            .map(|state| state.review.id.clone())
            .collect::<Vec<_>>();
        for review_id in active {
            self.handle_command(project, review_id, ReviewCommand::ResetForCleanWorkingTree)
                .await;
        }
    }

    pub(crate) async fn handle_registry(&mut self, project: &Project, command: RegistryCommand) {
        match command {
            RegistryCommand::Create { request, reply } => {
                let _ = reply.send(self.create(request).await);
            }
            RegistryCommand::Summaries { reply, .. } => {
                let _ = reply.send(Ok(self.summaries()));
            }
            RegistryCommand::UnsubscribeAll { conn } => {
                for state in self.reviews.values_mut() {
                    state.unsubscribe(&conn);
                }
            }
            RegistryCommand::Handle { review_id, reply } => {
                let result = self
                    .reviews
                    .get(&review_id)
                    .map(|state| state.handle.clone())
                    .ok_or_else(|| "Unknown review".to_owned());
                let _ = reply.send(result);
            }
            command => {
                let review_id = match &command {
                    RegistryCommand::Action { review_id, .. }
                    | RegistryCommand::Subscribe { review_id, .. }
                    | RegistryCommand::AiSuggestion { review_id, .. }
                    | RegistryCommand::BundleConsumed { review_id, .. }
                    | RegistryCommand::InternalError { review_id, .. } => review_id.clone(),
                    _ => {
                        reject_registry_command(
                            command,
                            "Invalid review operation in project owner".to_owned(),
                        );
                        return;
                    }
                };
                if !self.reviews.contains_key(&review_id) {
                    reject_registry_command(command, "Unknown review".to_owned());
                    return;
                }
                let action = match command {
                    RegistryCommand::Subscribe {
                        conn,
                        stream,
                        include_diffs,
                        reply,
                        ..
                    } => ReviewCommand::Subscribe {
                        conn,
                        stream,
                        include_diffs,
                        reply,
                    },
                    RegistryCommand::Action {
                        action,
                        conn,
                        reply,
                        ..
                    } => {
                        let _ = reply.send(Ok(()));
                        ReviewCommand::Action { action, conn }
                    }
                    RegistryCommand::AiSuggestion {
                        suggestion, reply, ..
                    } => {
                        let (inner, response) = oneshot::channel();
                        tokio::spawn(async move {
                            let _ = reply.send(response.await.map_err(|_| {
                                "project review suggestion response dropped".to_owned()
                            }));
                        });
                        ReviewCommand::AiSuggestion {
                            suggestion,
                            reply: inner,
                        }
                    }
                    RegistryCommand::BundleConsumed {
                        target_agent_id,
                        at_ms,
                        reply,
                        ..
                    } => {
                        let _ = reply.send(Ok(()));
                        ReviewCommand::BundleConsumed {
                            target_agent_id,
                            at_ms,
                        }
                    }
                    RegistryCommand::InternalError {
                        message,
                        context,
                        reply,
                        ..
                    } => {
                        let _ = reply.send(Ok(()));
                        ReviewCommand::InternalError { message, context }
                    }
                    _ => {
                        reject_registry_command(
                            command,
                            "Invalid routed review operation".to_owned(),
                        );
                        return;
                    }
                };
                self.handle_command(project, review_id, action).await;
            }
        }
    }

    async fn create(&mut self, request: ReviewCreateRequest) -> Result<ReviewId, String> {
        let is_active_workspace = active_review_selection(&request.selection);
        if matches!(
            request.selection,
            ReviewDiffSelection::CommittedRange { .. }
        ) {
            return Err("committed ranges are reviewed inside the workspace review".to_owned());
        }
        let mut existing = self
            .reviews
            .values()
            .map(|state| &state.review)
            .filter(|review| {
                matches!(review.status, ReviewStatus::Draft)
                    && if is_active_workspace {
                        active_review_selection(&review.selection)
                    } else {
                        match &request.selection {
                            ReviewDiffSelection::Root { root, .. } => {
                                active_review_root(review).as_ref() == Some(root)
                            }
                            _ => false,
                        }
                    }
            })
            .collect::<Vec<_>>();
        existing.sort_by(|left, right| active_review_sort(left, right));
        if let Some(review) = existing.first() {
            self.changed |= is_active_workspace;
            return Ok(review.id.clone());
        }
        let now = now_ms();
        let (origin_agent_id, origin_session_id) = synthetic_review_origin(&request.project_id);
        let review = Review {
            id: request.review_id.clone(),
            project_id: request.project_id,
            origin_agent_id,
            origin_session_id,
            selection: normalize_create_selection(request.selection),
            status: ReviewStatus::Draft,
            diffs: request.diffs,
            file_snapshots: Vec::new(),
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: protocol::ReviewAiReviewerState {
                rounds: Vec::new(),
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
                scope: protocol::ReviewAiScope::WorkingTree,
            },
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.registry.runtime.store.upsert(review.clone()).await?;
        self.insert(review)?;
        Ok(request.review_id)
    }
}

fn reset_running_ai_reviewer(review: &mut Review) -> bool {
    if matches!(review.ai_reviewer.status, ReviewAiReviewerStatus::Running) {
        if review.ai_reviewer.rounds.is_empty() {
            review.ai_reviewer.status = ReviewAiReviewerStatus::Idle;
            review.ai_reviewer.agent_id = None;
            review.ai_reviewer.error = None;
            review.updated_at_ms = now_ms();
            return true;
        }
        review.ai_reviewer.status = ReviewAiReviewerStatus::Failed;
        review.ai_reviewer.error =
            Some("Review interrupted by server restart; request a new review".to_owned());
        if let Some(round) = review.ai_reviewer.rounds.last_mut() {
            for reviewer in &mut round.reviewers {
                if reviewer.status == ReviewAiReviewerStatus::Running {
                    reviewer.status = ReviewAiReviewerStatus::Failed;
                    reviewer.error = review.ai_reviewer.error.clone();
                }
            }
        }
        review.updated_at_ms = now_ms();
        return true;
    }
    false
}

fn normalize_rehydrated_workspace_review(review: &mut Review) -> bool {
    if !matches!(review.status, ReviewStatus::Draft) {
        return false;
    }
    if !matches!(review.selection, ReviewDiffSelection::Workspace { .. }) {
        return false;
    }
    let normalized = active_workspace_selection();
    if review.selection == normalized {
        return false;
    }
    review.selection = normalized;
    review.updated_at_ms = now_ms();
    true
}

pub(crate) fn review_stream_path(review_id: &ReviewId) -> StreamPath {
    StreamPath(format!("/review/{}", review_id.0))
}

pub(crate) fn build_create_request(
    review_id: ReviewId,
    project_id: ProjectId,
    payload: ReviewCreatePayload,
    diffs: Vec<ProjectGitDiffPayload>,
) -> ReviewCreateRequest {
    ReviewCreateRequest {
        review_id,
        project_id,
        selection: normalize_create_selection(payload.selection),
        diffs: normalize_diff_payloads(diffs),
    }
}

fn synthetic_review_origin(project_id: &ProjectId) -> (AgentId, SessionId) {
    let id = format!("project-review:{}", project_id.0);
    (AgentId(id.clone()), SessionId(id))
}

pub(crate) fn review_create_selection(
    project: &Project,
    selection: &ReviewDiffSelection,
) -> Result<ReviewDiffSelection, String> {
    match selection {
        ReviewDiffSelection::Root { root, .. } => {
            if project
                .root_paths()
                .iter()
                .any(|candidate| candidate == root)
            {
                Ok(ReviewDiffSelection::Root {
                    root: root.clone(),
                    scope: ProjectDiffScope::Unstaged,
                    path: None,
                })
            } else {
                Err(format!(
                    "project {} does not contain review root {}",
                    project.id, root
                ))
            }
        }
        ReviewDiffSelection::CommittedRange { .. } => Err(
            "committed ranges are reviewed inside the workspace review: comment on the \
             committed diff instead of creating a review"
                .to_owned(),
        ),
        ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => {
            if project.root_paths().is_empty() {
                Err(format!("project {} has no review roots", project.id))
            } else {
                Ok(active_workspace_selection())
            }
        }
    }
}

fn normalize_create_selection(selection: ReviewDiffSelection) -> ReviewDiffSelection {
    match selection {
        ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => {
            active_workspace_selection()
        }
        ReviewDiffSelection::Root { root, .. } => ReviewDiffSelection::Root {
            root,
            scope: ProjectDiffScope::Unstaged,
            path: None,
        },
        ReviewDiffSelection::CommittedRange { .. } => selection,
    }
}

fn active_workspace_selection() -> ReviewDiffSelection {
    ReviewDiffSelection::Workspace {
        scope: ProjectDiffScope::Unstaged,
    }
}

fn active_review_selection(selection: &ReviewDiffSelection) -> bool {
    matches!(
        selection,
        ReviewDiffSelection::Workspace {
            scope: ProjectDiffScope::Unstaged
        }
    )
}

fn active_review_root(review: &Review) -> Option<ProjectRootPath> {
    match &review.selection {
        ReviewDiffSelection::Root {
            root, path: None, ..
        } => Some(root.clone()),
        ReviewDiffSelection::Root { .. } => None,
        ReviewDiffSelection::CommittedRange { .. } => None,
        ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => None,
    }
}

fn normalize_diff_payloads(mut diffs: Vec<ProjectGitDiffPayload>) -> Vec<ProjectGitDiffPayload> {
    for diff in &mut diffs {
        if matches!(diff.revision, protocol::ProjectDiffRevision::WorkingTree) {
            diff.scope = ProjectDiffScope::Unstaged;
        }
        diff.context_mode = DiffContextMode::FullFile;
    }
    diffs
}

fn active_review_sort(left: &Review, right: &Review) -> Ordering {
    right
        .updated_at_ms
        .cmp(&left.updated_at_ms)
        .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
        .then_with(|| left.id.0.cmp(&right.id.0))
}
