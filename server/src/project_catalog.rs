use super::*;
use protocol::{
    ProjectMutation, ProjectOperationPayload, ProjectOperationStage, ProjectOperationState,
    ProjectOperationWarning,
};
use std::collections::VecDeque;
use tokio::task::JoinSet;
use tracing::Instrument;

#[derive(Clone)]
struct CatalogSnapshot {
    store: Arc<ProjectStore>,
    generation: u64,
    operations: Vec<ProjectOperationPayload>,
}

pub(crate) struct ProjectCatalogHandle {
    tx: mpsc::UnboundedSender<Command>,
    snapshot: watch::Receiver<CatalogSnapshot>,
    completed: watch::Receiver<bool>,
}

pub(crate) struct ProjectBindingReservation {
    stream: StreamPath,
    tx: mpsc::UnboundedSender<Command>,
}

impl Drop for ProjectBindingReservation {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Release(self.stream.clone()));
    }
}

#[derive(Clone)]
pub(crate) enum CatalogResult {
    Project(Project),
    Workbench(CreatedWorkbench),
    Done,
}

type Completion = oneshot::Receiver<AppResult<CatalogResult>>;

struct Pending {
    progress: ProjectOperationPayload,
    resources: Vec<Resource>,
    result: oneshot::Sender<AppResult<CatalogResult>>,
    output: Option<Stream>,
    committed: Option<CatalogResult>,
    destructive: bool,
}

#[derive(Clone, Copy)]
enum FamilyUse {
    Mutation,
    Creation,
    Binding,
}

enum Resource {
    Family(ProjectId, FamilyUse),
    Root(ProjectRootPath),
    TopLevelOrder,
}

struct Prepared {
    mutation: ProjectMutation,
    roots: Vec<CreatedWorkbenchRoot>,
    environment: protocol::WorkbenchEnvironment,
    removal_phase: Option<protocol::WorkbenchRemovalPhase>,
    intent: Option<protocol::WorkbenchCreationIntent>,
    creation_warnings: Option<Vec<ProjectOperationWarning>>,
    intermediate: bool,
}

struct Commit {
    span: tracing::Span,
    queued: std::time::Instant,
    stream: StreamPath,
    prepared: Prepared,
    reply: oneshot::Sender<AppResult<(CatalogResult, Vec<ProjectOperationWarning>)>>,
}

enum Command {
    #[cfg(feature = "test-support")]
    HoldNextCreation {
        reached: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
        installed: oneshot::Sender<()>,
        prepared: bool,
    },
    Admit {
        mutation: ProjectMutation,
        base: Option<BaseRevision>,
        output: Option<Stream>,
        reply: oneshot::Sender<AppResult<Completion>>,
    },
    Bind {
        projects: Vec<ProjectId>,
        reply: oneshot::Sender<Result<ProjectBindingReservation, String>>,
    },
    Release(StreamPath),
    Published(StreamPath),
    Stage {
        stream: StreamPath,
        stage: ProjectOperationStage,
        destructive: bool,
    },
    Commit(Commit),
    EnterScheduledProject(lifecycle::ProjectEntryRequest),
    Finished {
        stream: StreamPath,
        result: AppResult<(CatalogResult, Vec<ProjectOperationWarning>)>,
    },
    #[cfg(feature = "test-support")]
    Crash(oneshot::Sender<()>),
    Stop,
}

#[cfg(feature = "test-support")]
impl HostHandle {
    pub async fn crash_project_catalog_for_test(&self) {
        let (reply, done) = oneshot::channel();
        self.project_catalog
            .tx
            .send(Command::Crash(reply))
            .expect("catalog running");
        done.await.expect("catalog crash completed");
    }

    pub async fn hold_next_workbench_creation_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        self.hold_workbench_creation_at_for_test(false).await
    }

    pub async fn hold_prepared_workbench_creation_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        self.hold_workbench_creation_at_for_test(true).await
    }

    async fn hold_workbench_creation_at_for_test(
        &self,
        prepared: bool,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, entered) = oneshot::channel();
        let (resume, release) = oneshot::channel();
        let (installed, ready) = oneshot::channel();
        self.project_catalog
            .tx
            .send(Command::HoldNextCreation {
                reached,
                release,
                installed,
                prepared,
            })
            .expect("catalog must be running while installing creation hook");
        ready.await.expect("catalog must install creation hook");
        (entered, resume)
    }
}

enum Publication {
    Progress(StreamPath),
    Final {
        progress: ProjectOperationPayload,
        output: Option<Stream>,
        error: Option<Box<protocol::CommandErrorPayload>>,
    },
    Commit {
        stream: StreamPath,
        generation: u64,
        mutation: Box<ProjectMutation>,
        changes: Vec<ProjectNotifyPayload>,
        final_commit: bool,
        settled: Option<ProjectOperationPayload>,
        reply: oneshot::Sender<Vec<ProjectOperationWarning>>,
    },
    Stop,
}

pub(crate) struct ProjectCatalogActor {
    handle: Weak<ProjectCatalogHandle>,
    snapshot: watch::Sender<CatalogSnapshot>,
    rx: mpsc::UnboundedReceiver<Command>,
    completed: watch::Sender<bool>,
}

impl ProjectCatalogHandle {
    pub(crate) fn new(store: ProjectStore) -> (Arc<Self>, ProjectCatalogActor) {
        let (tx, rx) = mpsc::unbounded_channel();
        let operations = operation_snapshot(&store, &HashMap::new());
        let (snapshot, current) = watch::channel(CatalogSnapshot {
            store: Arc::new(store),
            generation: 0,
            operations,
        });
        let (completed, completion) = watch::channel(false);
        let handle = Arc::new(Self {
            tx,
            snapshot: current,
            completed: completion,
        });
        let actor = ProjectCatalogActor {
            handle: Arc::downgrade(&handle),
            snapshot,
            rx,
            completed,
        };
        (handle, actor)
    }

    pub(crate) fn record(&self, id: &ProjectId) -> Option<protocol::ProjectCatalogRecord> {
        let snapshot = self.snapshot.borrow();
        snapshot
            .store
            .get(id)
            .map(|project| protocol::ProjectCatalogRecord {
                generation: snapshot.generation,
                project,
            })
    }
    pub(crate) fn get(&self, id: &ProjectId) -> Option<Project> {
        self.record(id).map(|record| record.project)
    }
    pub(crate) fn list(&self) -> Result<Vec<Project>, ProjectStoreError> {
        self.snapshot.borrow().store.list()
    }
    pub(crate) fn generation(&self) -> u64 {
        self.snapshot.borrow().generation
    }
    pub(crate) fn snapshot_creations(&self) -> Vec<protocol::WorkbenchCreationIntent> {
        self.snapshot.borrow().store.creations()
    }
    pub(crate) fn operations(&self) -> Vec<ProjectOperationPayload> {
        self.snapshot.borrow().operations.clone()
    }
    pub(crate) fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub(crate) async fn shutdown(&self) {
        self.stop();
        let mut complete = self.completed.clone();
        while !*complete.borrow_and_update() {
            if complete.changed().await.is_err() {
                tracing::error!("catalog shutdown completion was lost");
                return;
            }
        }
    }

    pub(super) fn published(&self, stream: StreamPath) {
        let _ = self.tx.send(Command::Published(stream));
    }

    pub(crate) fn is_removing(&self, id: &ProjectId) -> bool {
        let snapshot = self.snapshot.borrow();
        snapshot
            .store
            .get(id)
            .is_some_and(|project| project.is_removing_workbench())
            || snapshot
                .operations
                .iter()
                .any(|operation| match &operation.mutation {
                    ProjectMutation::Delete(payload) => &payload.id == id,
                    ProjectMutation::RemoveWorkbench(payload) => &payload.id == id,
                    _ => false,
                })
    }

    fn environment_directory(&self) -> AppResult<PathBuf> {
        let store = self.snapshot.borrow().store.clone();
        store.environment_directory()
    }

    pub(crate) fn enter_scheduled_project(
        &self,
        agent_id: AgentId,
        source: Option<ProjectId>,
        destination: Option<ProjectId>,
        workspace_roots: Vec<String>,
    ) -> Result<oneshot::Receiver<Result<lifecycle::ProjectEntryExecution, String>>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::EnterScheduledProject(
                lifecycle::ProjectEntryRequest {
                    agent_id,
                    source,
                    destination,
                    workspace_roots,
                    reply,
                },
            ))
            .map_err(|_| "Project catalog stopped before project entry".to_owned())?;
        Ok(response)
    }

    pub(super) async fn persist_removal_phase(
        &self,
        stream: &StreamPath,
        payload: &WorkbenchRemovePayload,
        phase: protocol::WorkbenchRemovalPhase,
    ) -> AppResult<()> {
        let (_, warnings) = self
            .commit(
                stream,
                Prepared {
                    mutation: ProjectMutation::RemoveWorkbench(payload.clone()),
                    roots: Vec::new(),
                    environment: protocol::WorkbenchEnvironment::Native,
                    removal_phase: Some(phase),
                    intent: None,
                    creation_warnings: None,
                    intermediate: false,
                },
            )
            .await?;
        if warnings.is_empty() {
            Ok(())
        } else {
            Err(AppError::internal(
                "workbench_remove",
                anyhow!(
                    "Removal phase persisted but publication failed; inspect the workbench before an explicit retry: {}",
                    warnings
                        .iter()
                        .map(|warning| warning.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ))
        }
    }

    pub(crate) async fn reserve_bindings(
        &self,
        projects: Vec<ProjectId>,
    ) -> Result<ProjectBindingReservation, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Bind { projects, reply })
            .map_err(|_| "project catalog stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project catalog stopped".to_owned())?
    }

    pub(crate) async fn admit(
        &self,
        mutation: ProjectMutation,
        base: Option<BaseRevision>,
        output: Option<Stream>,
    ) -> AppResult<Completion> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Admit {
                mutation,
                base,
                output,
                reply,
            })
            .map_err(|_| AppError::conflict("project_admission", "project catalog stopped"))?;
        response
            .await
            .map_err(|_| AppError::conflict("project_admission", "project catalog stopped"))?
    }

    fn stage(&self, stream: &StreamPath, stage: ProjectOperationStage, destructive: bool) {
        let _ = self.tx.send(Command::Stage {
            stream: stream.clone(),
            stage,
            destructive,
        });
    }

    async fn commit(
        &self,
        stream: &StreamPath,
        prepared: Prepared,
    ) -> AppResult<(CatalogResult, Vec<ProjectOperationWarning>)> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Commit(Commit {
                span: tracing::Span::current(),
                queued: std::time::Instant::now(),
                stream: stream.clone(),
                prepared,
                reply,
            }))
            .map_err(|_| AppError::conflict("project_commit", "project catalog stopped"))?;
        response
            .await
            .map_err(|_| AppError::conflict("project_commit", "project catalog stopped"))?
    }
}

impl ProjectCatalogActor {
    pub(crate) fn start(self, host: WeakHostHandle) -> Result<(), String> {
        crate::project_stream::spawn_project_task(self.run(host)).map(|_| ())
    }

    fn snapshot_operations(&self, active: &HashMap<StreamPath, Pending>) {
        self.snapshot.send_modify(|snapshot| {
            snapshot.operations = operation_snapshot(&snapshot.store, active)
        });
    }

    async fn run(mut self, host: WeakHostHandle) {
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let publisher = publish_events(host.clone(), self.snapshot.subscribe(), publish_rx);
        let mut publisher = Box::pin(publisher);
        let mut active = HashMap::<StreamPath, Pending>::new();
        let mut bindings = HashMap::<StreamPath, Vec<Resource>>::new();
        let mut workers = JoinSet::new();
        let mut entries = JoinSet::new();
        let mut worker_streams = HashMap::new();
        let mut commits = VecDeque::<Commit>::new();
        let mut persistence = JoinSet::new();
        let mut publications = JoinSet::new();
        let mut stopping = false;
        let mut draining = false;
        #[cfg(feature = "test-support")]
        let mut next_creation = None;
        let interrupted = self.snapshot.borrow().store.creations();
        for intent in interrupted {
            let mutation = ProjectMutation::RecoverWorkbench(protocol::WorkbenchRecoverPayload {
                operation: intent.operation.clone(),
                parent_project_id: intent.payload.parent_project_id.clone(),
            });
            let Some(handle) = self.handle.upgrade() else {
                break;
            };
            let Some(worker_host) = host.upgrade() else {
                break;
            };
            let resources = creation_resources(&intent);
            let stream = intent.operation.clone();
            let (result, _) = oneshot::channel();
            active.insert(
                stream.clone(),
                Pending {
                    progress: ProjectOperationPayload {
                        host_id: HostFilterId(LOCAL_HOST_ID.into()),
                        stream: stream.clone(),
                        mutation: mutation.clone(),
                        state: ProjectOperationState::Admitted,
                    },
                    resources,
                    result,
                    output: None,
                    committed: None,
                    destructive: true,
                },
            );
            let task_stream = stream.clone();
            let abort = workers.spawn(async move {
                run_mutation(
                    &worker_host,
                    &handle,
                    &task_stream,
                    mutation,
                    None,
                    #[cfg(feature = "test-support")]
                    None,
                )
                .await
            });
            worker_streams.insert(abort.id(), stream);
        }
        self.snapshot_operations(&active);
        loop {
            if persistence.is_empty()
                && publications.is_empty()
                && let Some(commit) = commits.pop_front()
            {
                let store = self.snapshot.borrow().store.as_ref().clone();
                let operation = mutation_name(&commit.prepared.mutation);
                tracing::info!(target: "tyde::project_operation", operation, stage = "persist", queue_count = commits.len(), "catalog persistence started");
                let Commit {
                    span,
                    queued,
                    stream,
                    prepared,
                    reply,
                } = commit;
                let final_commit = prepared.removal_phase.is_none() && !prepared.intermediate;
                let receipt_only = prepared.creation_warnings.is_some();
                persistence.spawn_blocking(move || span.in_scope(move || {
                    let started = std::time::Instant::now();
                    let outcome = persist(store, &stream, prepared);
                    tracing::info!(target: "tyde::project_operation", operation, stage = "persist", success = outcome.is_ok(), queue_ms = started.duration_since(queued).as_millis(), elapsed_ms = started.elapsed().as_millis(), "catalog persistence finished");
                    (stream, reply, final_commit, receipt_only, outcome)
                }));
            }
            if stopping
                && !draining
                && active.is_empty()
                && entries.is_empty()
                && workers.is_empty()
                && persistence.is_empty()
                && publications.is_empty()
                && commits.is_empty()
            {
                draining = true;
                let _ = publish_tx.send(Publication::Stop);
            }
            tokio::select! {
                _ = &mut publisher => {
                    if !draining { tracing::error!("catalog publisher stopped unexpectedly"); }
                    break;
                }
                command = self.rx.recv(), if !draining => {
                    let Some(command) = command else { stopping = true; continue; };
                    match command {
                        #[cfg(feature = "test-support")]
                        Command::Crash(reply) => {
                            workers.abort_all();
                            while workers.join_next().await.is_some() {}
                            let _ = reply.send(());
                            break;
                        }
                        Command::Stop => { stopping = true; }
                        Command::Finished { stream, result } => {
                    if let Some(mut pending) = active.remove(&stream) {
                        let mut error_payload = None;
                        pending.progress.state = match &result {
                            Ok((_, warnings)) => ProjectOperationState::Committed { warnings: warnings.clone() },
                            Err(error) if pending.committed.is_some() => ProjectOperationState::Committed { warnings: vec![ProjectOperationWarning { stage: ProjectOperationStage::Fanout, message: error.message.clone() }] },
                            Err(error) => {
                                if let Some(output) = &pending.output {
                                    let mut payload = error.to_payload(output.path().clone(), mutation_kind(&pending.progress.mutation));
                                    payload.context = error_context(&pending.progress.mutation);
                                    error_payload = Some(Box::new(payload));
                                }
                                ProjectOperationState::Failed { code: error.code(), message: error.message.clone(), cleanup_may_be_partial: match &pending.progress.mutation { ProjectMutation::CreateWorkbench(_) => self.snapshot.borrow().store.creation(&stream).is_some(), _ => pending.destructive } }
                            }
                        };
                        if let ProjectMutation::RecoverWorkbench(payload) = &pending.progress.mutation
                            && payload.operation == stream
                            && result.is_ok()
                            && let Some(outcome) = self.snapshot.borrow().store.creation_outcomes().iter().find(|outcome| outcome.stream == stream) {
                            pending.progress = outcome.clone();
                        }
                        let post_commit_error = match &pending.progress.state {
                            ProjectOperationState::Committed { warnings } if !warnings.is_empty() => {
                                let details = warnings.iter().map(|warning| format!("{:?}: {}", warning.stage, warning.message)).collect::<Vec<_>>().join("; ");
                                let error = AppError::internal(mutation_name(&pending.progress.mutation), anyhow!("Catalog committed (operation {stream}); do not repeat this mutation. Inspect the catalog and repair derived state: {details}"));
                                if let Some(output) = &pending.output {
                                    let mut payload = error.to_payload(output.path().clone(), mutation_kind(&pending.progress.mutation));
                                    payload.context = Some(protocol::CommandErrorContext::ProjectMutationCommitted { mutation: pending.progress.mutation.clone(), warnings: warnings.clone() });
                                    error_payload = Some(Box::new(payload));
                                }
                                Some(error)
                            }
                            _ => None,
                        };
                        tracing::info!(target: "tyde::project_operation", operation = mutation_name(&pending.progress.mutation), operation_stream = %stream, committed = pending.committed.is_some(), stage = "complete", "catalog operation settled");
                        self.snapshot_operations(&active);
                        let _ = publish_tx.send(Publication::Final { progress: pending.progress, output: pending.output, error: error_payload });
                        let result = match (post_commit_error, result, pending.committed) {
                            (Some(error), _, _) => Err(error),
                            (None, Ok((result, _)), _) => Ok(result),
                            (None, Err(_), Some(result)) => Ok(result),
                            (None, Err(error), None) => Err(error),
                        };
                        let _ = pending.result.send(result);
                    }
                        }
                        Command::EnterScheduledProject(request) => {
                            if stopping { let _ = request.reply.send(Err("Project catalog is stopping".into())); continue; }
                            let Some(worker_host) = host.upgrade() else { let _ = request.reply.send(Err("Host stopped before project entry".into())); continue; };
                            let Some(handle) = self.handle.upgrade() else { let _ = request.reply.send(Err("Project catalog stopped before project entry".into())); continue; };
                            entries.spawn(lifecycle::run_entry(worker_host, handle, request));
                        }
                        #[cfg(feature = "test-support")]
                        Command::HoldNextCreation { reached, release, installed, prepared } => {
                            assert!(next_creation.is_none(), "creation hook already installed");
                            next_creation = Some((reached, release, prepared));
                            let _ = installed.send(());
                        }
                        Command::Admit { mutation, base, output, reply } => {
                            let operation = mutation_name(&mutation);
                            if stopping { let _ = reply.send(Err(AppError::conflict(operation, "project catalog is stopping"))); continue; }
                            let Some(handle) = self.handle.upgrade() else { let _ = reply.send(Err(AppError::conflict(operation, "project catalog stopped"))); continue; };
                            let resources = match resources(&handle, &mutation) { Ok(resources) => resources, Err(error) => { let _ = reply.send(Err(error)); continue; } };
                            let durable = durable_resources(&handle.snapshot.borrow().store, match &mutation { ProjectMutation::RecoverWorkbench(payload) => Some(&payload.operation), _ => None });
                            if conflicts(&resources, active.values().flat_map(|pending| &pending.resources).chain(bindings.values().flatten()).chain(durable.iter())) {
                                tracing::info!(target: "tyde::project_operation", operation, stage = "admission", accepted = false, "catalog resource conflict");
                                let _ = reply.send(Err(AppError::conflict(operation, "project resources have a conflicting mutation or binding in flight"))); continue;
                            }
                            let Some(worker_host) = host.upgrade() else { let _ = reply.send(Err(AppError::conflict(operation, "host stopped"))); continue; };
                            let stream = StreamPath(format!("/project-operation/{}", Uuid::new_v4()));
                            let progress = ProjectOperationPayload { host_id: HostFilterId(LOCAL_HOST_ID.to_owned()), stream: stream.clone(), mutation: mutation.clone(), state: ProjectOperationState::Admitted };
                            let (result, response) = oneshot::channel();
                            tracing::info!(target: "tyde::project_operation", operation, stage = "admission", resource_count = resources.len(), active_count = active.len() + 1, accepted = true, "catalog mutation admitted");
                            active.insert(stream.clone(), Pending { progress, resources, result, output, committed: None, destructive: false });
                            self.snapshot_operations(&active);
                            let _ = publish_tx.send(Publication::Progress(stream.clone()));
                            let task_stream = stream.clone();
                            let span = tracing::info_span!(target: "tyde::project_operation", "project_mutation", operation, operation_stream = %stream);
                            #[cfg(feature = "test-support")]
                            let creation_hook = matches!(mutation, ProjectMutation::CreateWorkbench(_)).then(|| next_creation.take()).flatten();
                            let abort = workers.spawn(async move { run_mutation(&worker_host, &handle, &task_stream, mutation, base, #[cfg(feature = "test-support")] creation_hook).await }.instrument(span));
                            worker_streams.insert(abort.id(), stream);
                            let _ = reply.send(Ok(response));
                        }
                        Command::Bind { projects, reply } => {
                            let Some(handle) = self.handle.upgrade() else { let _ = reply.send(Err("project catalog stopped".into())); continue; };
                            let result = if stopping { Err("project catalog is stopping".into()) }
                            else if projects.iter().any(|id| handle.get(id).is_some_and(|project| project.is_removing_workbench())) { Err("Workbench removal is in progress".into()) }
                            else {
                                match projects.iter().map(|id| family(&handle, id, FamilyUse::Binding)).collect::<AppResult<Vec<_>>>() {
                                    Err(error) => Err(error.message),
                                    Ok(resources) if conflicts(&resources, active.values().flat_map(|pending| &pending.resources).chain(durable_resources(&handle.snapshot.borrow().store, None).iter())) => Err("project is being removed or mutated by an admitted operation".into()),
                                    Ok(resources) => {
                                        let stream = StreamPath(format!("/project-binding/{}", Uuid::new_v4()));
                                        bindings.insert(stream.clone(), resources);
                                        Ok(ProjectBindingReservation { stream, tx: handle.tx.clone() })
                                    }
                                }
                            };
                            tracing::info!(target: "tyde::project_operation", operation = "project_bind", stage = "admission", accepted = result.is_ok(), project_count = projects.len(), active_count = active.len(), "catalog binding admission completed");
                            let _ = reply.send(result);
                        }
                        Command::Release(stream) => { bindings.remove(&stream); }
                        Command::Published(stream) => { if let Some(pending) = active.get_mut(&stream) { pending.resources.clear(); } }
                        Command::Stage { stream, stage, destructive } => {
                            if let Some(pending) = active.get_mut(&stream) {
                                pending.progress.state = ProjectOperationState::Running { stage };
                                pending.destructive |= destructive;
                                tracing::info!(target: "tyde::project_operation", operation = mutation_name(&pending.progress.mutation), operation_stream = %stream, ?stage, "catalog worker phase");
                                self.snapshot_operations(&active);
                                let _ = publish_tx.send(Publication::Progress(stream));
                            }
                        }
                        Command::Commit(commit) => {
                            if active.contains_key(&commit.stream) { commits.push_back(commit); }
                            else { let _ = commit.reply.send(Err(AppError::protocol("project_commit", "mutation reservation is missing"))); }
                        }
                    }
                }
                Some(joined) = persistence.join_next(), if !persistence.is_empty() => {
                    match joined {
                        Ok((stream, reply, final_commit, receipt_only, Ok((store, changes, result, settled)))) => {
                            if let Some(pending) = active.get_mut(&stream) {
                                if final_commit { pending.committed = Some(result.clone()); }
                                if !receipt_only { pending.progress.state = ProjectOperationState::Running { stage: ProjectOperationStage::Fanout }; }
                            }
                            let generation = self.snapshot.borrow().generation + 1;
                            let operations = operation_snapshot(&store, &active);
                            self.snapshot.send_modify(|snapshot| { snapshot.store = Arc::new(store); snapshot.generation = generation; snapshot.operations = operations; });
                            if receipt_only {
                                let _ = reply.send(Ok((result, Vec::new())));
                                continue;
                            }
                            let (published, response) = oneshot::channel();
                            if publish_tx.send(Publication::Commit { stream: stream.clone(), generation, mutation: Box::new(active[&stream].progress.mutation.clone()), changes, final_commit, settled, reply: published }).is_err() {
                                let _ = reply.send(Ok((result, vec![ProjectOperationWarning { stage: ProjectOperationStage::Fanout, message: "catalog committed but publisher stopped".into() }])));
                                continue;
                            }
                            publications.spawn(async move {
                                let warnings = response.await.unwrap_or_else(|_| vec![ProjectOperationWarning { stage: ProjectOperationStage::Fanout, message: "catalog committed but publication failed".into() }]);
                                let _ = reply.send(Ok((result, warnings)));
                            });
                        }
                        Ok((_, reply, _, _, Err(error))) => { let _ = reply.send(Err(error)); }
                        Err(error) => tracing::error!(%error, "catalog persistence worker failed"),
                    }
                }
                Some(joined) = publications.join_next(), if !publications.is_empty() => {
                    if let Err(error) = joined { tracing::error!(%error, "catalog publication completion failed"); }
                }
                Some(joined) = entries.join_next(), if !entries.is_empty() => {
                    if let Err(error) = joined { tracing::error!(%error, "Project entry worker failed"); }
                }
                Some(joined) = workers.join_next_with_id(), if !workers.is_empty() => {
                    let (stream, result) = match joined {
                        Ok((id, result)) => {
                            let Some(stream) = worker_streams.remove(&id) else { continue; };
                            (stream, result)
                        }
                        Err(error) => {
                            let Some(stream) = worker_streams.remove(&error.id()) else { continue; };
                            (stream, Err(AppError::internal("project_operation", anyhow!(error))))
                        }
                    };
                    if let Some(handle) = self.handle.upgrade() {
                        let _ = handle.tx.send(Command::Finished { stream, result });
                    }

                }
            }
        }
        self.completed.send_replace(true);
    }
}

fn family(catalog: &ProjectCatalogHandle, id: &ProjectId, usage: FamilyUse) -> AppResult<Resource> {
    let project = catalog.get(id).ok_or_else(|| {
        AppError::not_found(
            "project_admission",
            "cannot use missing project; destination is missing or was removed before entry",
        )
    })?;
    Ok(Resource::Family(
        project.parent_project_id().unwrap_or(id).clone(),
        usage,
    ))
}

fn resources(
    catalog: &ProjectCatalogHandle,
    mutation: &ProjectMutation,
) -> AppResult<Vec<Resource>> {
    let mut held = Vec::new();
    match mutation {
        ProjectMutation::Create(payload) => {
            held.extend(payload.roots.iter().cloned().map(Resource::Root));
        }
        ProjectMutation::Delete(payload) => {
            let project = catalog.get(&payload.id).ok_or_else(|| {
                AppError::not_found(
                    "project_delete",
                    format!("cannot delete missing project {}", payload.id),
                )
            })?;
            if project.is_workbench() {
                return Err(AppError::invalid(
                    "project_delete",
                    "use WorkbenchRemove for workbenches",
                ));
            }
            if let Some(child) = catalog
                .list()
                .map_err(|error| project_store_error("project_delete", error))?
                .iter()
                .find(|project| project.parent_project_id() == Some(&payload.id))
            {
                return Err(AppError::conflict(
                    "project_delete",
                    format!(
                        "cannot delete project {} while referenced by workbench {}",
                        payload.id, child.id
                    ),
                ));
            }
            held.push(family(catalog, &payload.id, FamilyUse::Mutation)?);
            held.extend(project.root_paths().into_iter().map(Resource::Root));
        }
        ProjectMutation::AddRoot(payload) => {
            held.push(family(catalog, &payload.id, FamilyUse::Mutation)?);
            held.push(Resource::Root(payload.root.clone()));
        }
        ProjectMutation::DeleteRoot(payload) => {
            held.push(family(catalog, &payload.id, FamilyUse::Mutation)?);
            held.push(Resource::Root(payload.root.clone()));
        }
        ProjectMutation::Rename(payload) => {
            held.push(family(catalog, &payload.id, FamilyUse::Mutation)?);
        }
        ProjectMutation::RemoveWorkbench(payload) => {
            held.push(family(catalog, &payload.id, FamilyUse::Mutation)?);
        }
        ProjectMutation::RecoverWorkbench(payload) => {
            let intent = catalog
                .snapshot
                .borrow()
                .store
                .creation(&payload.operation)
                .ok_or_else(|| {
                    AppError::not_found(
                        "workbench_recover",
                        "No interrupted creation exists for this operation",
                    )
                })?;
            if intent.payload.parent_project_id != payload.parent_project_id {
                return Err(AppError::conflict(
                    "workbench_recover",
                    "Creation belongs to another parent project",
                ));
            }
            held.extend(creation_resources(&intent));
        }
        ProjectMutation::CreateWorkbench(payload) => {
            held.push(family(
                catalog,
                &payload.parent_project_id,
                FamilyUse::Creation,
            )?);
            let parent = catalog.get(&payload.parent_project_id).ok_or_else(|| {
                AppError::not_found("workbench_create", "parent project does not exist")
            })?;
            if parent.is_workbench() {
                return Err(AppError::invalid(
                    "workbench_create",
                    "cannot create workbench for non-standalone parent project",
                ));
            }
            held.extend(
                compute_workbench_roots(&parent.root_paths(), &payload.branch)?
                    .into_iter()
                    .map(|root| Resource::Root(root.worktree_root)),
            );
        }
        ProjectMutation::Reorder(payload) => match &payload.scope {
            protocol::ProjectReorderScope::TopLevel => {
                held.push(Resource::TopLevelOrder);
                held.extend(
                    catalog
                        .list()
                        .map_err(|error| project_store_error("project_reorder", error))?
                        .iter()
                        .filter(|project| !project.is_workbench())
                        .map(|project| Resource::Family(project.id.clone(), FamilyUse::Mutation)),
                );
            }
            protocol::ProjectReorderScope::WorkbenchChildren { parent_project_id } => {
                held.push(family(catalog, parent_project_id, FamilyUse::Mutation)?)
            }
        },
    }
    Ok(held)
}

fn conflicts<'a>(resources: &[Resource], held: impl Iterator<Item = &'a Resource>) -> bool {
    held.into_iter().any(|held| {
        resources.iter().any(|resource| match resource {
            Resource::Family(id, usage) => match held {
                Resource::Family(held_id, held_usage) => {
                    id == held_id
                        && match (usage, held_usage) {
                            (FamilyUse::Mutation, _)
                            | (_, FamilyUse::Mutation)
                            | (FamilyUse::Creation, FamilyUse::Creation) => true,
                            (FamilyUse::Creation, FamilyUse::Binding)
                            | (FamilyUse::Binding, FamilyUse::Creation)
                            | (FamilyUse::Binding, FamilyUse::Binding) => false,
                        }
                }
                Resource::Root(_) | Resource::TopLevelOrder => false,
            },
            Resource::Root(root) => matches!(held, Resource::Root(held_root) if root == held_root),
            Resource::TopLevelOrder => matches!(held, Resource::TopLevelOrder),
        })
    })
}

fn mutation_name(mutation: &ProjectMutation) -> &'static str {
    match mutation {
        ProjectMutation::Create(_) => "project_create",
        ProjectMutation::Delete(_) => "project_delete",
        ProjectMutation::AddRoot(_) => "project_add_root",
        ProjectMutation::DeleteRoot(_) => "project_delete_root",
        ProjectMutation::CreateWorkbench(_) => "workbench_create",
        ProjectMutation::RemoveWorkbench(_) => "workbench_remove",
        ProjectMutation::RecoverWorkbench(_) => "workbench_recover",
        ProjectMutation::Rename(_) => "project_rename",
        ProjectMutation::Reorder(_) => "project_reorder",
    }
}

fn mutation_kind(mutation: &ProjectMutation) -> FrameKind {
    match mutation {
        ProjectMutation::Create(_) => FrameKind::ProjectCreate,
        ProjectMutation::Delete(_) => FrameKind::ProjectDelete,
        ProjectMutation::AddRoot(_) => FrameKind::ProjectAddRoot,
        ProjectMutation::DeleteRoot(_) => FrameKind::ProjectDeleteRoot,
        ProjectMutation::CreateWorkbench(_) => FrameKind::WorkbenchCreate,
        ProjectMutation::RemoveWorkbench(_) => FrameKind::WorkbenchRemove,
        ProjectMutation::RecoverWorkbench(_) => FrameKind::WorkbenchRecover,
        ProjectMutation::Rename(_) => FrameKind::ProjectRename,
        ProjectMutation::Reorder(_) => FrameKind::ProjectReorder,
    }
}

fn error_context(mutation: &ProjectMutation) -> Option<protocol::CommandErrorContext> {
    match mutation {
        ProjectMutation::CreateWorkbench(payload) => {
            Some(protocol::CommandErrorContext::WorkbenchCreate {
                parent_project_id: payload.parent_project_id.clone(),
                branch: payload.branch.clone(),
            })
        }
        ProjectMutation::RemoveWorkbench(payload) => {
            Some(protocol::CommandErrorContext::WorkbenchRemove {
                project_id: payload.id.clone(),
            })
        }
        _ => Some(protocol::CommandErrorContext::ProjectMutation {
            mutation: mutation.clone(),
        }),
    }
}

fn persist(
    mut store: ProjectStore,
    stream: &StreamPath,
    prepared: Prepared,
) -> AppResult<(
    ProjectStore,
    Vec<ProjectNotifyPayload>,
    CatalogResult,
    Option<ProjectOperationPayload>,
)> {
    let operation = mutation_name(&prepared.mutation);
    let mut changes = Vec::new();
    let mut settled = None;
    if let Some(warnings) = prepared.creation_warnings {
        if !matches!(prepared.mutation, ProjectMutation::CreateWorkbench(_)) {
            return Err(AppError::protocol(
                operation,
                "Only committed creation has completion receipts",
            ));
        }
        store
            .settle_creation(stream, warnings)
            .map_err(|error| project_store_error(operation, error))?;
        return Ok((store, changes, CatalogResult::Done, None));
    }
    if let Some(intent) = prepared.intent {
        store
            .begin_creation(intent)
            .map_err(|error| project_store_error(operation, error))?;
        return Ok((store, changes, CatalogResult::Done, None));
    }
    if let Some(phase) = prepared.removal_phase {
        let ProjectMutation::RemoveWorkbench(payload) = prepared.mutation else {
            return Err(AppError::protocol(
                operation,
                "Only workbench removal has persisted phases",
            ));
        };
        let project = store
            .set_workbench_lifecycle(
                &payload.id,
                protocol::WorkbenchEnvironmentLifecycle::Removing { phase },
            )
            .map_err(|error| project_store_error(operation, error))?;
        changes.push(ProjectNotifyPayload::Upsert { project });
        return Ok((store, changes, CatalogResult::Done, None));
    }
    let result = match prepared.mutation {
        ProjectMutation::Create(payload) => {
            let project = store
                .create(payload.name, payload.roots)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Upsert {
                project: project.clone(),
            });
            CatalogResult::Project(project)
        }
        ProjectMutation::Delete(payload) => {
            let project = store
                .delete(&payload.id)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Delete { project });
            CatalogResult::Done
        }
        ProjectMutation::AddRoot(payload) => {
            let project = store
                .add_root(&payload.id, payload.root)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Upsert { project });
            CatalogResult::Done
        }
        ProjectMutation::DeleteRoot(payload) => {
            let project = store
                .delete_root(&payload.id, &payload.root)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Upsert { project });
            CatalogResult::Done
        }
        ProjectMutation::Rename(payload) => {
            let project = store
                .rename(&payload.id, payload.name)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Upsert { project });
            CatalogResult::Done
        }
        ProjectMutation::Reorder(payload) => {
            for project in store
                .reorder(payload.scope, payload.project_ids)
                .map_err(|error| project_store_error(operation, error))?
            {
                changes.push(ProjectNotifyPayload::Upsert { project });
            }
            CatalogResult::Done
        }
        ProjectMutation::RecoverWorkbench(payload) => {
            let outcome = store
                .rollback_creation(&payload.operation)
                .map_err(|error| project_store_error(operation, error))?;
            if &outcome.stream != stream {
                settled = Some(outcome);
            }
            CatalogResult::Done
        }
        ProjectMutation::CreateWorkbench(_) => {
            let project = store
                .create_workbench(stream, prepared.environment)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Upsert {
                project: project.clone(),
            });
            CatalogResult::Workbench(CreatedWorkbench {
                project,
                roots: prepared.roots,
            })
        }
        ProjectMutation::RemoveWorkbench(payload) => {
            let project = store
                .delete_workbench(&payload.id)
                .map_err(|error| project_store_error(operation, error))?;
            changes.push(ProjectNotifyPayload::Delete { project });
            CatalogResult::Done
        }
    };
    Ok((store, changes, result, settled))
}

async fn run_mutation(
    host: &HostHandle,
    catalog: &ProjectCatalogHandle,
    stream: &StreamPath,
    mutation: ProjectMutation,
    base: Option<BaseRevision>,
    #[cfg(feature = "test-support")] creation_hook: Option<(
        oneshot::Sender<()>,
        oneshot::Receiver<()>,
        bool,
    )>,
) -> AppResult<(CatalogResult, Vec<ProjectOperationWarning>)> {
    let operation = mutation_name(&mutation);
    catalog.stage(stream, ProjectOperationStage::Queue, false);
    let mut resolved = Vec::new();
    let mut creation_intent = None;
    let mut environment = protocol::WorkbenchEnvironment::Native;
    if let ProjectMutation::CreateWorkbench(payload) = &mutation {
        catalog.stage(stream, ProjectOperationStage::Git, false);
        let parent = catalog
            .get(&payload.parent_project_id)
            .ok_or_else(|| AppError::not_found(operation, "parent project does not exist"))?;
        let engine = host
            .selected_workbench_engine()
            .await
            .map_err(|error| AppError::internal(operation, anyhow!(error)))?;
        let environment_directory = match engine {
            protocol::WorkbenchEngine::Native => None,
            protocol::WorkbenchEngine::Bubblewrap => Some(catalog.environment_directory()?),
        };
        let roots = compute_workbench_roots(&parent.root_paths(), &payload.branch)?;
        preflight_workbench_create(catalog, &parent, &payload.branch, &roots).await?;
        for mut root in roots {
            let path = Path::new(&root.worktree_root.0);
            let parent = path.parent().ok_or_else(|| {
                AppError::invalid(operation, "Workbench source has no parent directory")
            })?;
            let name = path.file_name().ok_or_else(|| {
                AppError::invalid(operation, "Workbench source has no directory name")
            })?;
            let canonical = tokio::fs::canonicalize(parent)
                .await
                .map_err(|error| AppError::internal(operation, error))?
                .join(name);
            root.worktree_root = ProjectRootPath(
                canonical
                    .to_str()
                    .ok_or_else(|| {
                        AppError::invalid(operation, "Canonical workbench source must be UTF-8")
                    })?
                    .to_owned(),
            );
            resolved.push(CreatedWorkbenchRoot {
                base_commit: git_resolve_base_commit(&root.parent_root, base.as_ref()).await?,
                parent_root_dirty: !git_status_porcelain(&root.parent_root)
                    .await
                    .map_err(|error| AppError::internal(operation, anyhow!(error)))?
                    .is_empty(),
                root,
            });
        }
        let intent = protocol::WorkbenchCreationIntent {
            operation: stream.clone(),
            payload: payload.clone(),
            roots: resolved.clone(),
            environment_directory: environment_directory
                .as_ref()
                .map(|path| {
                    path.to_str()
                        .map(|value| protocol::HostAbsPath(value.to_owned()))
                        .ok_or_else(|| {
                            AppError::invalid(operation, "Environment path must be UTF-8")
                        })
                })
                .transpose()?,
        };
        catalog
            .commit(
                stream,
                Prepared {
                    mutation: mutation.clone(),
                    roots: Vec::new(),
                    environment: protocol::WorkbenchEnvironment::Native,
                    removal_phase: None,
                    intent: Some(intent.clone()),
                    creation_warnings: None,
                    intermediate: true,
                },
            )
            .await?;
        creation_intent = Some(intent.clone());
        catalog.stage(stream, ProjectOperationStage::Filesystem, true);
        let preparation = async {
            for root in &resolved {
                claim_creation_root(&intent, root).await?;
            }
            #[cfg(feature = "test-support")]
            let creation_hook = match creation_hook {
                Some((reached, release, false)) => {
                    let _ = reached.send(());
                    let _ = release.await;
                    None
                }
                hook => hook,
            };
            if let Some(directory) = environment_directory {
                catalog.stage(stream, ProjectOperationStage::PrepareBuilds, true);
                let roots = resolved
                    .iter()
                    .map(|root| root.root.clone())
                    .collect::<Vec<_>>();
                environment =
                    crate::workbench::prepare(&roots, &directory, creation_claim(&intent)?).await?;
            }
            #[cfg(feature = "test-support")]
            if let Some((reached, release, _)) = creation_hook {
                let _ = reached.send(());
                let _ = release.await;
            }
            Ok::<_, AppError>(())
        }
        .await;
        if let Err(mut error) = preparation {
            match rollback_creation(&intent).await {
                Ok(()) => {
                    let recovery =
                        ProjectMutation::RecoverWorkbench(protocol::WorkbenchRecoverPayload {
                            operation: stream.clone(),
                            parent_project_id: payload.parent_project_id.clone(),
                        });
                    if let Err(cleanup) = catalog
                        .commit(
                            stream,
                            Prepared {
                                mutation: recovery,
                                roots: Vec::new(),
                                environment: protocol::WorkbenchEnvironment::Native,
                                removal_phase: None,
                                intent: None,
                                creation_warnings: None,
                                intermediate: true,
                            },
                        )
                        .await
                    {
                        error.message = format!(
                            "{}; cleanup receipt failed: {}",
                            error.message, cleanup.message
                        );
                    }
                }
                Err(cleanup) => {
                    error.message = format!(
                        "{}; owned rollback incomplete: {}. Retry WorkbenchRecover for operation {}",
                        error.message, cleanup.message, stream
                    );
                }
            }
            return Err(error);
        }
    }
    if let ProjectMutation::RecoverWorkbench(payload) = &mutation {
        let intent = catalog
            .snapshot
            .borrow()
            .store
            .creation(&payload.operation)
            .ok_or_else(|| AppError::not_found(operation, "Creation intent is missing"))?;
        catalog.stage(stream, ProjectOperationStage::Cleanup, true);
        rollback_creation(&intent).await?;
    }
    match &mutation {
        ProjectMutation::Delete(payload) => {
            catalog.stage(stream, ProjectOperationStage::Cleanup, true);
            host.prepare_project_delete(&payload.id, false).await?;
        }
        ProjectMutation::RemoveWorkbench(payload) => {
            wait_for_workbench_remove_test_hook(host).await;
            let project = catalog
                .get(&payload.id)
                .ok_or_else(|| AppError::not_found(operation, "cannot remove missing project"))?;
            catalog.stage(stream, ProjectOperationStage::Git, false);
            host.validate_workbench_remove_roots(&project, payload.force)
                .await?;
            catalog.stage(stream, ProjectOperationStage::Cleanup, true);
            host.prepare_workbench_remove(payload, &project, catalog, stream)
                .await?;
        }
        _ => {}
    }
    catalog.stage(stream, ProjectOperationStage::Persistence, false);
    let result = catalog
        .commit(
            stream,
            Prepared {
                mutation: mutation.clone(),
                roots: resolved,
                environment: environment.clone(),
                removal_phase: None,
                intent: None,
                creation_warnings: None,
                intermediate: false,
            },
        )
        .await;
    let (result, mut warnings) = result?;
    if let Some(intent) = creation_intent
        && let Err(error) = release_creation_claims(&intent).await
    {
        warnings.push(ProjectOperationWarning {
            stage: ProjectOperationStage::Cleanup,
            message: error.message,
        });
    }
    catalog.stage(stream, ProjectOperationStage::Workflow, false);
    if let Err(error) = host
        .update_workflow_watcher_targets_and_reload(operation)
        .await
    {
        tracing::error!(target: "tyde::project_operation", operation, stage = "workflow", "derived workflow update failed after commit");
        warnings.push(ProjectOperationWarning {
            stage: ProjectOperationStage::Workflow,
            message: error.message,
        });
    }
    if matches!(mutation, ProjectMutation::CreateWorkbench(_))
        && let Err(error) = catalog
            .commit(
                stream,
                Prepared {
                    mutation,
                    roots: Vec::new(),
                    environment: protocol::WorkbenchEnvironment::Native,
                    removal_phase: None,
                    intent: None,
                    creation_warnings: Some(warnings.clone()),
                    intermediate: true,
                },
            )
            .await
    {
        warnings.push(ProjectOperationWarning {
            stage: ProjectOperationStage::Persistence,
            message: format!(
                "Catalog committed but completion receipt failed: {}",
                error.message
            ),
        });
    }
    Ok((result, warnings))
}

async fn publish_events(
    host: WeakHostHandle,
    snapshot: watch::Receiver<CatalogSnapshot>,
    mut rx: mpsc::UnboundedReceiver<Publication>,
) {
    while let Some(event) = rx.recv().await {
        if matches!(event, Publication::Stop) {
            return;
        }
        let Some(host) = host.upgrade() else {
            return;
        };
        match event {
            Publication::Progress(stream) => {
                let snapshot = snapshot.clone();
                host.state
                    .with(move |state| {
                        if let Some(progress) = snapshot
                            .borrow()
                            .operations
                            .iter()
                            .find(|operation| operation.stream == stream)
                        {
                            publish_progress(state, progress);
                        }
                    })
                    .await
            }
            Publication::Final {
                progress,
                output,
                error,
            } => {
                host.state
                    .with(move |state| {
                        publish_progress(state, &progress);
                        if let (Some(output), Some(error)) = (output, error) {
                            match serde_json::to_value(error) {
                                Ok(value) => {
                                    let _ = output.send_value(FrameKind::CommandError, value);
                                }
                                Err(error) => {
                                    tracing::error!(%error, "cannot serialize mutation error")
                                }
                            }
                        }
                    })
                    .await
            }
            Publication::Commit {
                stream,
                generation,
                mutation,
                changes,
                final_commit,
                settled,
                reply,
            } => {
                let warnings = host
                    .publish_project_catalog(stream, generation, *mutation, changes, final_commit)
                    .await;
                if let Some(settled) = settled {
                    host.state
                        .with(move |state| publish_progress(state, &settled))
                        .await;
                }
                let _ = reply.send(warnings);
            }
            Publication::Stop => return,
        }
    }
}

pub(super) fn publish_progress(state: &mut HostState, progress: &ProjectOperationPayload) {
    match serde_json::to_value(progress) {
        Ok(value) => state.host_streams.retain(|_, subscriber| {
            emit_or_queue_host_frame(subscriber, FrameKind::ProjectOperation, value.clone()).is_ok()
        }),
        Err(error) => tracing::error!(%error, "cannot serialize mutation progress"),
    }
}
fn creation_claim(intent: &protocol::WorkbenchCreationIntent) -> AppResult<&str> {
    let id = intent
        .operation
        .0
        .strip_prefix("/project-operation/")
        .ok_or_else(|| AppError::invalid("workbench_recover", "Invalid creation operation"))?;
    Uuid::parse_str(id).map_err(|error| AppError::internal("workbench_recover", error))?;
    Ok(id)
}

fn creation_owner_ref(intent: &protocol::WorkbenchCreationIntent) -> AppResult<String> {
    Ok(format!(
        "refs/tyde/workbench-creation/{}",
        creation_claim(intent)?
    ))
}

async fn git_transaction(root: &ProjectRootPath, commands: &str) -> AppResult<()> {
    use std::io::Write;
    let mut input = tempfile::NamedTempFile::new()
        .map_err(|error| AppError::internal("workbench_create", error))?;
    write!(input, "start\noption no-deref\n{commands}prepare\ncommit\n")
        .map_err(|error| AppError::internal("workbench_create", error))?;
    let output = crate::process_env::command("git")
        .map_err(|error| AppError::internal("workbench_create", anyhow!(error)))?
        .arg("-C")
        .arg(&root.0)
        .args(["update-ref", "--stdin"])
        .stdin(
            input
                .reopen()
                .map_err(|error| AppError::internal("workbench_create", error))?,
        )
        .output()
        .await
        .map_err(|error| AppError::internal("workbench_create", error))?;
    if !output.status.success() {
        return Err(AppError::internal(
            "workbench_create",
            anyhow!(
                "Git ownership transaction failed: {}",
                git_output_message(&output)
            ),
        ));
    }
    Ok(())
}

async fn claim_creation_root(
    intent: &protocol::WorkbenchCreationIntent,
    root: &CreatedWorkbenchRoot,
) -> AppResult<()> {
    let owner = creation_owner_ref(intent)?;
    git_transaction(
        &root.root.parent_root,
        &format!(
            "create refs/heads/{} {}\ncreate {} {}\n",
            intent.payload.branch.0, root.base_commit, owner, root.base_commit
        ),
    )
    .await?;
    let output = run_git(
        &root.root.parent_root,
        &[
            "worktree",
            "add",
            &root.root.worktree_root.0,
            &intent.payload.branch.0,
        ],
    )
    .await
    .map_err(|error| AppError::internal("workbench_create", anyhow!(error)))?;
    if !output.status.success() {
        return Err(AppError::internal(
            "workbench_create",
            anyhow!(
                "Git worktree creation failed: {}",
                git_output_message(&output)
            ),
        ));
    }
    Ok(())
}

async fn checked_git_ref(root: &ProjectRootPath, reference: &str) -> AppResult<Option<String>> {
    let output = run_git(root, &["rev-parse", "--verify", "--quiet", reference])
        .await
        .map_err(|error| AppError::internal("workbench_recover", anyhow!(error)))?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(|value| Some(value.trim().to_owned()))
            .map_err(|error| AppError::internal("workbench_recover", error)),
        Some(1) => Ok(None),
        _ => Err(AppError::conflict(
            "workbench_recover",
            "Cannot establish Git ref ownership",
        )),
    }
}

async fn creation_worktree_registered(
    root: &CreatedWorkbenchRoot,
    branch: &str,
) -> AppResult<bool> {
    let listing = run_git(
        &root.root.parent_root,
        &["worktree", "list", "--porcelain", "-z"],
    )
    .await
    .map_err(|error| AppError::internal("workbench_recover", anyhow!(error)))?;
    if !listing.status.success() {
        return Err(AppError::conflict(
            "workbench_recover",
            "Cannot establish worktree ownership",
        ));
    }
    let path = format!("worktree {}", root.root.worktree_root.0);
    let head = format!("HEAD {}", root.base_commit);
    let branch = format!("branch {branch}");
    let fields = listing.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let registrations = fields
        .split(|field| field.is_empty())
        .filter(|record| record.contains(&path.as_bytes()) || record.contains(&branch.as_bytes()))
        .collect::<Vec<_>>();
    match registrations.as_slice() {
        [] => Ok(false),
        [record] if record.contains(&branch.as_bytes()) && !record.contains(&path.as_bytes()) => {
            Err(AppError::conflict(
                "workbench_recover",
                "Creation branch has a relocated Git worktree; resources preserved",
            ))
        }
        [record]
            if record.contains(&path.as_bytes())
                && record.contains(&head.as_bytes())
                && record.contains(&branch.as_bytes()) =>
        {
            Ok(true)
        }
        _ => Err(AppError::conflict(
            "workbench_recover",
            "Source is not the claimed Git worktree; resources preserved",
        )),
    }
}

async fn rollback_creation_environment(directory: &Path, claim: &str) -> AppResult<()> {
    #[derive(serde::Deserialize)]
    struct EnvironmentClaim {
        claim: String,
    }

    let metadata = match tokio::fs::symlink_metadata(directory).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(AppError::internal("workbench_recover", error)),
    };
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(AppError::conflict(
            "workbench_recover",
            "Unowned environment path preserved",
        ));
    }
    let ownership_error = || {
        AppError::conflict(
            "workbench_recover",
            "Environment internal ownership claim missing, unreadable or different; resources preserved",
        )
    };
    let claim_path = directory.join("claim.json");
    let metadata = tokio::fs::symlink_metadata(&claim_path)
        .await
        .map_err(|_| ownership_error())?;
    if !metadata.is_file() || metadata.is_symlink() {
        return Err(ownership_error());
    }
    let contents = tokio::fs::read(&claim_path)
        .await
        .map_err(|_| ownership_error())?;
    let recorded: EnvironmentClaim =
        serde_json::from_slice(&contents).map_err(|_| ownership_error())?;
    if recorded.claim != claim {
        return Err(ownership_error());
    }
    tokio::fs::remove_dir_all(directory)
        .await
        .map_err(|error| AppError::internal("workbench_recover", error))
}

async fn rollback_creation(intent: &protocol::WorkbenchCreationIntent) -> AppResult<()> {
    let owner = creation_owner_ref(intent)?;
    if let Some(directory) = &intent.environment_directory {
        let directory = Path::new(&directory.0);
        let claim = creation_claim(intent)?;
        let staging = directory
            .parent()
            .ok_or_else(|| AppError::invalid("workbench_recover", "Environment has no parent"))?
            .join(format!(".claim-{claim}"));
        rollback_creation_environment(directory, claim).await?;
        rollback_creation_environment(&staging, claim).await?;
    }
    for root in intent.roots.iter().rev() {
        let branch = format!("refs/heads/{}", intent.payload.branch.0);
        let claim = checked_git_ref(&root.root.parent_root, &owner).await?;
        let branch_commit = checked_git_ref(&root.root.parent_root, &branch).await?;
        let metadata = match tokio::fs::symlink_metadata(&root.root.worktree_root.0).await {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(AppError::internal("workbench_recover", error)),
        };
        let exists = metadata.is_some();
        let registered = creation_worktree_registered(root, &branch).await?;
        match claim {
            None if !exists && !registered && branch_commit.is_none() => continue,
            Some(ref commit) if commit == &root.base_commit => {}
            _ => {
                return Err(AppError::conflict(
                    "workbench_recover",
                    "Git ownership claim missing or changed; resources preserved",
                ));
            }
        }
        if branch_commit
            .as_ref()
            .is_some_and(|commit| commit != &root.base_commit)
        {
            return Err(AppError::conflict(
                "workbench_recover",
                "Creation branch advanced; resources preserved",
            ));
        }
        if metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.is_dir() || metadata.is_symlink())
        {
            return Err(AppError::conflict(
                "workbench_recover",
                "Unexpected source path preserved",
            ));
        }
        if !registered && exists {
            return Err(AppError::conflict(
                "workbench_recover",
                "Source is not the claimed Git worktree; resources preserved",
            ));
        }
        if registered {
            git_worktree_remove(&root.root.parent_root, &root.root.worktree_root, false)
                .await
                .map_err(|error| AppError::conflict("workbench_recover", error))?;
            if creation_worktree_registered(root, &branch).await? {
                return Err(AppError::conflict(
                    "workbench_recover",
                    "Owned Git registration remains; creation intent retained",
                ));
            }
        }
        let mut commands = format!("delete {owner} {}\n", root.base_commit);
        if branch_commit.is_some() {
            commands.push_str(&format!("delete {branch} {}\n", root.base_commit));
        }
        git_transaction(&root.root.parent_root, &commands).await?;
    }
    tracing::info!(target: "tyde::project_operation", root_count = intent.roots.len(), "Interrupted creation owned resources rolled back");
    Ok(())
}

async fn release_creation_claims(intent: &protocol::WorkbenchCreationIntent) -> AppResult<()> {
    let owner = creation_owner_ref(intent)?;
    for root in &intent.roots {
        git_transaction(
            &root.root.parent_root,
            &format!("delete {owner} {}\n", root.base_commit),
        )
        .await?;
    }
    Ok(())
}

fn creation_resources(intent: &protocol::WorkbenchCreationIntent) -> Vec<Resource> {
    std::iter::once(Resource::Family(
        intent.payload.parent_project_id.clone(),
        FamilyUse::Creation,
    ))
    .chain(
        intent
            .roots
            .iter()
            .map(|root| Resource::Root(root.root.worktree_root.clone())),
    )
    .collect()
}

fn durable_resources(store: &ProjectStore, except: Option<&StreamPath>) -> Vec<Resource> {
    store
        .creations()
        .iter()
        .filter(|intent| Some(&intent.operation) != except)
        .flat_map(creation_resources)
        .collect()
}

fn operation_snapshot(
    store: &ProjectStore,
    active: &HashMap<StreamPath, Pending>,
) -> Vec<ProjectOperationPayload> {
    let mut operations = store
        .creation_outcomes()
        .iter()
        .filter(|outcome| !active.contains_key(&outcome.stream))
        .cloned()
        .collect::<Vec<_>>();
    operations.extend(store.creations().into_iter().filter(|intent| !active.contains_key(&intent.operation)).map(|intent| ProjectOperationPayload {
        host_id: HostFilterId(LOCAL_HOST_ID.into()), stream: intent.operation.clone(),
        mutation: ProjectMutation::RecoverWorkbench(protocol::WorkbenchRecoverPayload { operation: intent.operation, parent_project_id: intent.payload.parent_project_id }),
        state: ProjectOperationState::Failed { code: protocol::CommandErrorCode::Conflict, message: "Interrupted creation requires owned rollback; retry recovery after resolving the reported ownership or Git error".into(), cleanup_may_be_partial: true },
    }));
    operations.extend(active.values().map(|pending| pending.progress.clone()));
    operations
}
