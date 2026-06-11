pub(crate) mod actor;
pub(crate) mod bundle;
pub(crate) mod reviewer;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use actor::{ConnectionId, ReviewCommand, spawn_review_actor};
use protocol::{
    AgentId, DiffContextMode, Project, ProjectDiffScope, ProjectGitDiffPayload, ProjectId,
    ProjectRootPath, Review, ReviewActionPayload, ReviewAiReviewerStatus, ReviewCreatePayload,
    ReviewDiffSelection, ReviewErrorContext, ReviewErrorPayload, ReviewId, ReviewStatus,
    ReviewSuggestionId, ReviewSummary, SessionId, StreamPath,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

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
    async fn subscribe(
        &self,
        conn: ConnectionId,
        stream: Stream,
        include_diffs: bool,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ReviewCommand::Subscribe {
                conn,
                stream,
                include_diffs,
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

    async fn reset_for_clean_working_tree(&self) -> Result<(), String> {
        self.tx
            .send(ReviewCommand::ResetForCleanWorkingTree)
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
}

#[derive(Clone)]
pub(crate) struct ReviewRegistryHandle {
    tx: mpsc::Sender<RegistryCommand>,
}

pub(crate) struct ReviewCreateRequest {
    pub review_id: ReviewId,
    pub project_id: ProjectId,
    pub selection: ReviewDiffSelection,
    pub diffs: Vec<ProjectGitDiffPayload>,
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
    ResetProjectRootsForCleanUnstaged {
        project_id: ProjectId,
        roots: Vec<ProjectRootPath>,
        reply: oneshot::Sender<Result<(), String>>,
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
        project_store: Arc<Mutex<ProjectStore>>,
        delivery_tx: mpsc::Sender<ReviewDeliveryRequest>,
        ai_spawn_tx: mpsc::Sender<ReviewAiSpawnRequest>,
        project_update_tx: mpsc::UnboundedSender<ProjectId>,
    ) -> Result<ReviewRegistryHandle, String> {
        let mut handles = HashMap::new();
        let mut rehydrated = Vec::new();
        for mut review in store.list()? {
            let mut changed = reset_running_ai_reviewer(&mut review);
            changed |= normalize_rehydrated_workspace_review(&mut review);
            if changed {
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

    pub(crate) async fn reset_project_roots_for_clean_unstaged(
        &self,
        project_id: ProjectId,
        roots: Vec<ProjectRootPath>,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RegistryCommand::ResetProjectRootsForCleanUnstaged {
                project_id,
                roots,
                reply,
            })
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped clean reset response".to_owned())?
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
            .await
            .map_err(|_| "review registry stopped".to_owned())?;
        response
            .await
            .map_err(|_| "review registry dropped delete-for-project response".to_owned())?
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
                    include_diffs,
                    reply,
                } => {
                    let result = self.subscribe(review_id, conn, stream, include_diffs).await;
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
                RegistryCommand::ResetProjectRootsForCleanUnstaged {
                    project_id,
                    roots,
                    reply,
                } => {
                    let result = self
                        .reset_project_roots_for_clean_unstaged(project_id, roots)
                        .await;
                    let _ = reply.send(result);
                }
                RegistryCommand::DeleteForProject { project_id, reply } => {
                    let result = self.delete_for_project(&project_id);
                    let _ = reply.send(result);
                }
            }
        }
    }

    async fn create(&mut self, request: ReviewCreateRequest) -> Result<ReviewId, String> {
        let is_active_workspace = active_review_selection(&request.selection);
        let existing = if is_active_workspace {
            self.draft_workspace_review_for_project(&request.project_id)
                .await?
        } else {
            match &request.selection {
                ReviewDiffSelection::Root { root, .. } => {
                    self.draft_review_for_project_root(&request.project_id, root)
                        .await?
                }
                ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => None,
            }
        };
        if let Some(review_id) = existing {
            if is_active_workspace {
                let _ = self.project_update_tx.send(request.project_id);
            }
            return Ok(review_id);
        }

        let now = now_ms();
        let review_id = request.review_id;
        let (origin_agent_id, origin_session_id) = synthetic_review_origin(&request.project_id);
        let review = Review {
            id: review_id.clone(),
            project_id: request.project_id.clone(),
            origin_agent_id,
            origin_session_id,
            selection: normalize_create_selection(request.selection),
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
        self.handles.insert(review_id.clone(), handle);
        if is_active_workspace {
            let _ = self.project_update_tx.send(request.project_id);
        }
        Ok(review_id)
    }

    async fn draft_workspace_review_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<Option<ReviewId>, String> {
        let handles = self.handles.values().cloned().collect::<Vec<_>>();
        let mut drafts = Vec::new();
        for handle in handles {
            let snapshot = handle.snapshot().await?;
            if snapshot.project_id == *project_id
                && matches!(snapshot.status, ReviewStatus::Draft)
                && active_review_selection(&snapshot.selection)
            {
                drafts.push(snapshot);
            }
        }
        drafts.sort_by(active_review_sort);
        Ok(drafts.into_iter().next().map(|review| review.id))
    }

    async fn draft_review_for_project_root(
        &self,
        project_id: &ProjectId,
        root: &ProjectRootPath,
    ) -> Result<Option<ReviewId>, String> {
        let handles = self.handles.values().cloned().collect::<Vec<_>>();
        let mut drafts = Vec::new();
        for handle in handles {
            let snapshot = handle.snapshot().await?;
            if snapshot.project_id == *project_id
                && matches!(snapshot.status, ReviewStatus::Draft)
                && active_review_root(&snapshot).as_ref() == Some(root)
            {
                drafts.push(snapshot);
            }
        }
        drafts.sort_by(active_review_sort);
        Ok(drafts.into_iter().next().map(|review| review.id))
    }

    async fn subscribe(
        &mut self,
        review_id: ReviewId,
        conn: ConnectionId,
        stream: Stream,
        include_diffs: bool,
    ) -> Result<(), String> {
        let handle = self
            .handles
            .get(&review_id)
            .ok_or_else(|| format!("unknown review {}", review_id))?;
        handle.subscribe(conn, stream, include_diffs).await
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

    async fn summaries(&mut self, project_id: ProjectId) -> Result<Vec<ReviewSummary>, String> {
        let project = self.load_project(&project_id).await?;
        self.ensure_active_review_for_project(&project).await?;

        let handles = self.handles.values().cloned().collect::<Vec<_>>();
        let mut drafts = Vec::new();
        for handle in &handles {
            let snapshot = handle.snapshot().await?;
            if snapshot.project_id == project_id
                && matches!(snapshot.status, ReviewStatus::Draft)
                && active_review_selection(&snapshot.selection)
            {
                drafts.push(snapshot);
            }
        }
        drafts.sort_by(active_review_sort);
        Ok(drafts
            .into_iter()
            .next()
            .map(|review| vec![actor::summary_for_review(&review)])
            .unwrap_or_default())
    }

    async fn reset_project_roots_for_clean_unstaged(
        &self,
        project_id: ProjectId,
        roots: Vec<ProjectRootPath>,
    ) -> Result<(), String> {
        let project = self.load_project(&project_id).await?;
        let all_roots_clean = project
            .root_paths()
            .into_iter()
            .all(|root| roots.contains(&root));
        if !all_roots_clean {
            return Ok(());
        }
        let handles = self.handles.values().cloned().collect::<Vec<_>>();
        for handle in handles {
            let snapshot = handle.snapshot().await?;
            if snapshot.project_id == project_id
                && matches!(snapshot.status, ReviewStatus::Draft)
                && active_review_selection(&snapshot.selection)
            {
                handle.reset_for_clean_working_tree().await?;
            }
        }
        Ok(())
    }

    fn delete_for_project(&mut self, project_id: &ProjectId) -> Result<Vec<ReviewId>, String> {
        let removed = self.store.delete_for_project(project_id)?;
        for review_id in &removed {
            self.handles.remove(review_id);
        }
        Ok(removed)
    }

    async fn ensure_active_review_for_project(&mut self, project: &Project) -> Result<(), String> {
        if self
            .draft_workspace_review_for_project(&project.id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let review_id = ReviewId(Uuid::new_v4().to_string());
        let request = ReviewCreateRequest {
            review_id,
            project_id: project.id.clone(),
            selection: active_workspace_selection(),
            diffs: Vec::new(),
        };
        self.create(request).await?;
        Ok(())
    }

    async fn load_project(&self, project_id: &ProjectId) -> Result<Project, String> {
        let store = self.project_store.lock().await;
        store
            .get(project_id)
            .ok_or_else(|| format!("unknown project {}", project_id))
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
        ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => None,
    }
}

fn normalize_diff_payloads(mut diffs: Vec<ProjectGitDiffPayload>) -> Vec<ProjectGitDiffPayload> {
    for diff in &mut diffs {
        diff.scope = ProjectDiffScope::Unstaged;
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

#[cfg(test)]
mod tests {
    use protocol::{
        AgentId, ProjectDiffScope, ProjectGitDiffPayload, ProjectId, ProjectRootPath,
        ReviewAiReviewerState, ReviewDiffSelection, SessionId,
    };

    use super::*;
    use crate::review::actor::{event_for_subscriber, review_for_subscriber, summary_for_review};

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
    fn summary_counts_file_comment_semantics() {
        let review = Review {
            id: ReviewId("review-1".to_owned()),
            project_id: ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: Vec::new(),
            comments: vec![
                protocol::ReviewComment {
                    id: protocol::ReviewCommentId("comment-1".to_owned()),
                    location: protocol::ReviewLocation {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: "src/lib.rs".to_owned(),
                        anchor: protocol::ReviewAnchor::File,
                    },
                    anchor_status: protocol::ReviewAnchorStatus::Current,
                    body: "human".to_owned(),
                    source: protocol::ReviewCommentSource::User,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
                protocol::ReviewComment {
                    id: protocol::ReviewCommentId("comment-2".to_owned()),
                    location: protocol::ReviewLocation {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: "src/lib.rs".to_owned(),
                        anchor: protocol::ReviewAnchor::File,
                    },
                    anchor_status: protocol::ReviewAnchorStatus::Stale {
                        reason: "file moved".to_owned(),
                    },
                    body: "accepted AI".to_owned(),
                    source: protocol::ReviewCommentSource::AiSuggestion {
                        suggestion_id: protocol::ReviewSuggestionId(
                            "suggestion-accepted".to_owned(),
                        ),
                        edited: false,
                    },
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
                protocol::ReviewComment {
                    id: protocol::ReviewCommentId("comment-3".to_owned()),
                    location: protocol::ReviewLocation {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: "src/other.rs".to_owned(),
                        anchor: protocol::ReviewAnchor::File,
                    },
                    anchor_status: protocol::ReviewAnchorStatus::Current,
                    body: "other".to_owned(),
                    source: protocol::ReviewCommentSource::User,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
            ],
            suggestions: vec![
                protocol::ReviewSuggestedComment {
                    id: protocol::ReviewSuggestionId("suggestion-1".to_owned()),
                    location: protocol::ReviewLocation {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: "src/lib.rs".to_owned(),
                        anchor: protocol::ReviewAnchor::File,
                    },
                    anchor_status: protocol::ReviewAnchorStatus::Stale {
                        reason: "line moved".to_owned(),
                    },
                    body: "pending".to_owned(),
                    rationale: None,
                    severity: protocol::ReviewSeverity::Info,
                    state: protocol::ReviewSuggestionState::Pending,
                    reviewer_agent_id: AgentId("agent-2".to_owned()),
                    created_at_ms: 1,
                },
                protocol::ReviewSuggestedComment {
                    id: protocol::ReviewSuggestionId("suggestion-2".to_owned()),
                    location: protocol::ReviewLocation {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: "src/lib.rs".to_owned(),
                        anchor: protocol::ReviewAnchor::File,
                    },
                    anchor_status: protocol::ReviewAnchorStatus::Current,
                    body: "rejected".to_owned(),
                    rationale: None,
                    severity: protocol::ReviewSeverity::Info,
                    state: protocol::ReviewSuggestionState::Rejected,
                    reviewer_agent_id: AgentId("agent-2".to_owned()),
                    created_at_ms: 1,
                },
                protocol::ReviewSuggestedComment {
                    id: protocol::ReviewSuggestionId("suggestion-accepted".to_owned()),
                    location: protocol::ReviewLocation {
                        root: ProjectRootPath("/repo".to_owned()),
                        relative_path: "src/lib.rs".to_owned(),
                        anchor: protocol::ReviewAnchor::File,
                    },
                    anchor_status: protocol::ReviewAnchorStatus::Current,
                    body: "accepted".to_owned(),
                    rationale: None,
                    severity: protocol::ReviewSeverity::Info,
                    state: protocol::ReviewSuggestionState::Accepted {
                        comment_id: protocol::ReviewCommentId("comment-2".to_owned()),
                    },
                    reviewer_agent_id: AgentId("agent-2".to_owned()),
                    created_at_ms: 1,
                },
            ],
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 2,
        };

        let summary = summary_for_review(&review);
        assert_eq!(summary.user_comment_count, 3);
        assert_eq!(summary.pending_suggestion_count, 1);
        assert_eq!(summary.file_comment_counts.len(), 2);
        assert_eq!(
            summary.file_comment_counts[0].root,
            ProjectRootPath("/repo".to_owned())
        );
        assert_eq!(summary.file_comment_counts[0].relative_path, "src/lib.rs");
        assert_eq!(summary.file_comment_counts[0].user_comment_count, 1);
        assert_eq!(summary.file_comment_counts[0].ai_comment_count, 1);
        assert_eq!(summary.file_comment_counts[0].pending_suggestion_count, 1);
        assert_eq!(summary.file_comment_counts[0].total_count(), 3);
        assert_eq!(
            summary.file_comment_counts[1].root,
            ProjectRootPath("/repo".to_owned())
        );
        assert_eq!(summary.file_comment_counts[1].relative_path, "src/other.rs");
        assert_eq!(summary.file_comment_counts[1].user_comment_count, 1);
        assert_eq!(summary.file_comment_counts[1].ai_comment_count, 0);
        assert_eq!(summary.file_comment_counts[1].pending_suggestion_count, 0);
    }

    #[test]
    fn lightweight_review_subscriber_payloads_redact_diffs() {
        let review = Review {
            id: ReviewId("review-1".to_owned()),
            project_id: ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Unstaged,
                path: None,
                context_mode: DiffContextMode::FullFile,
                files: Vec::new(),
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 2,
        };

        assert_eq!(review_for_subscriber(&review, true).diffs.len(), 1);
        let lightweight = review_for_subscriber(&review, false);
        assert!(lightweight.diffs.is_empty());
        assert_eq!(lightweight.id, review.id);

        let event = event_for_subscriber(
            &protocol::ReviewEventPayload::Cleared {
                review: review.clone(),
            },
            false,
        );
        let protocol::ReviewEventPayload::Cleared { review } = event else {
            panic!("expected cleared event");
        };
        assert!(review.diffs.is_empty());

        let event = event_for_subscriber(
            &protocol::ReviewEventPayload::Snapshot {
                review: review.clone(),
            },
            false,
        );
        let protocol::ReviewEventPayload::Snapshot { review } = event else {
            panic!("expected snapshot event");
        };
        assert!(review.diffs.is_empty());
    }
}
