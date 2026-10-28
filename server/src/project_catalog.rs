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
    Stop,
}

#[cfg(feature = "test-support")]
impl HostHandle {
    pub async fn hold_next_workbench_creation_for_test(
        &self,
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
        let (snapshot, current) = watch::channel(CatalogSnapshot {
            store: Arc::new(store),
            generation: 0,
            operations: Vec::new(),
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
            snapshot.operations = active
                .values()
                .map(|pending| pending.progress.clone())
                .collect()
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
                let final_commit = prepared.removal_phase.is_none();
                persistence.spawn_blocking(move || span.in_scope(move || {
                    let started = std::time::Instant::now();
                    let outcome = persist(store, prepared);
                    tracing::info!(target: "tyde::project_operation", operation, stage = "persist", success = outcome.is_ok(), queue_ms = started.duration_since(queued).as_millis(), elapsed_ms = started.elapsed().as_millis(), "catalog persistence finished");
                    (stream, reply, final_commit, outcome)
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
                                ProjectOperationState::Failed { code: error.code(), message: error.message.clone(), cleanup_may_be_partial: pending.destructive }
                            }
                        };
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
                        Command::HoldNextCreation { reached, release, installed } => {
                            assert!(next_creation.is_none(), "creation hook already installed");
                            next_creation = Some((reached, release));
                            let _ = installed.send(());
                        }
                        Command::Admit { mutation, base, output, reply } => {
                            let operation = mutation_name(&mutation);
                            if stopping { let _ = reply.send(Err(AppError::conflict(operation, "project catalog is stopping"))); continue; }
                            let Some(handle) = self.handle.upgrade() else { let _ = reply.send(Err(AppError::conflict(operation, "project catalog stopped"))); continue; };
                            let resources = match resources(&handle, &mutation) { Ok(resources) => resources, Err(error) => { let _ = reply.send(Err(error)); continue; } };
                            if conflicts(&resources, active.values().flat_map(|pending| &pending.resources).chain(bindings.values().flatten())) {
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
                                    Ok(resources) if conflicts(&resources, active.values().flat_map(|pending| &pending.resources)) => Err("project is being removed or mutated by an admitted operation".into()),
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
                        Ok((stream, reply, final_commit, Ok((store, changes, result)))) => {
                            if let Some(pending) = active.get_mut(&stream) {
                                if final_commit { pending.committed = Some(result.clone()); }
                                pending.progress.state = ProjectOperationState::Running { stage: ProjectOperationStage::Fanout };
                            }
                            let generation = self.snapshot.borrow().generation + 1;
                            let operations = active.values().map(|pending| pending.progress.clone()).collect();
                            self.snapshot.send_modify(|snapshot| { snapshot.store = Arc::new(store); snapshot.generation = generation; snapshot.operations = operations; });
                            let (published, response) = oneshot::channel();
                            if publish_tx.send(Publication::Commit { stream: stream.clone(), generation, mutation: Box::new(active[&stream].progress.mutation.clone()), changes, final_commit, reply: published }).is_err() {
                                let _ = reply.send(Ok((result, vec![ProjectOperationWarning { stage: ProjectOperationStage::Fanout, message: "catalog committed but publisher stopped".into() }])));
                                continue;
                            }
                            publications.spawn(async move {
                                let warnings = response.await.unwrap_or_else(|_| vec![ProjectOperationWarning { stage: ProjectOperationStage::Fanout, message: "catalog committed but publication failed".into() }]);
                                let _ = reply.send(Ok((result, warnings)));
                            });
                        }
                        Ok((_, reply, _, Err(error))) => { let _ = reply.send(Err(error)); }
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
    prepared: Prepared,
) -> AppResult<(ProjectStore, Vec<ProjectNotifyPayload>, CatalogResult)> {
    let operation = mutation_name(&prepared.mutation);
    let mut changes = Vec::new();
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
        return Ok((store, changes, CatalogResult::Done));
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
        ProjectMutation::CreateWorkbench(payload) => {
            let roots = prepared
                .roots
                .iter()
                .map(|root| root.root.clone())
                .collect();
            let project = store
                .create_workbench(
                    payload.parent_project_id,
                    payload.name,
                    payload.branch,
                    roots,
                    prepared.environment,
                )
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
    Ok((store, changes, result))
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
    )>,
) -> AppResult<(CatalogResult, Vec<ProjectOperationWarning>)> {
    let operation = mutation_name(&mutation);
    catalog.stage(stream, ProjectOperationStage::Queue, false);
    let mut resolved = Vec::new();
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
        for root in roots {
            resolved.push(CreatedWorkbenchRoot {
                base_commit: git_resolve_base_commit(&root.parent_root, base.as_ref()).await?,
                parent_root_dirty: !git_status_porcelain(&root.parent_root)
                    .await
                    .map_err(|error| AppError::internal(operation, anyhow!(error)))?
                    .is_empty(),
                root,
            });
        }
        catalog.stage(stream, ProjectOperationStage::Filesystem, true);
        let mut created = Vec::new();
        for root in &resolved {
            if let Err(error) = git_worktree_add(
                &root.root.parent_root,
                &root.root.worktree_root,
                &payload.branch,
                &root.base_commit,
            )
            .await
            {
                let rollback = rollback_created_worktrees(&created, &payload.branch).await;
                return Err(AppError::internal(
                    operation,
                    anyhow!(append_rollback_message(error, rollback)),
                ));
            }
            created.push(root.root.clone());
        }
        #[cfg(feature = "test-support")]
        if let Some((reached, release)) = creation_hook {
            let _ = reached.send(());
            let _ = release.await;
        }
        if let Some(directory) = environment_directory {
            catalog.stage(stream, ProjectOperationStage::PrepareBuilds, true);
            match crate::workbench::prepare(&created, &directory).await {
                Ok(prepared) => environment = prepared,
                Err(mut error) => {
                    let environment_cleanup = rollback_workbench_environment(&directory).await;
                    let git_cleanup = rollback_created_worktrees(&created, &payload.branch).await;
                    error.message = append_rollback_message(
                        append_rollback_message(error.message, environment_cleanup),
                        git_cleanup,
                    );
                    return Err(error);
                }
            }
        }
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
    let created_roots = resolved
        .iter()
        .map(|resolved| resolved.root.clone())
        .collect::<Vec<_>>();
    let result = catalog
        .commit(
            stream,
            Prepared {
                mutation: mutation.clone(),
                roots: resolved,
                environment: environment.clone(),
                removal_phase: None,
            },
        )
        .await;
    let (result, mut warnings) = match result {
        Ok(result) => result,
        Err(error) => {
            if let ProjectMutation::CreateWorkbench(payload) = &mutation {
                let environment_cleanup = crate::workbench::finish(&environment)
                    .await
                    .err()
                    .map(|error| format!("Environment rollback failed: {error}"));
                let rollback = rollback_created_worktrees(&created_roots, &payload.branch).await;
                let mut error = error;
                error.message = append_rollback_message(
                    append_rollback_message(error.message, environment_cleanup),
                    rollback,
                );
                return Err(error);
            }
            return Err(error);
        }
    };
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
                reply,
            } => {
                let warnings = host
                    .publish_project_catalog(stream, generation, *mutation, changes, final_commit)
                    .await;
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
