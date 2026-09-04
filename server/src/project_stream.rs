use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{
    CodeIntelOverviewHeadline, CodeIntelOverviewPayload, CodeIntelOverviewSummary,
    CodeIntelProviderStatus, CodeIntelRootOverview, CodeIntelState, CommandErrorCode,
    CommandErrorPayload, DiffContextMode, FileEntryOp, FrameKind, Project,
    ProjectBinaryFilePayload, ProjectDiffRevision, ProjectDiffScope, ProjectEventPayload,
    ProjectFileContentsPayload, ProjectFileEntry, ProjectFileKind, ProjectFileListPayload,
    ProjectFileVersion, ProjectFileVersionChange, ProjectGitChangeKind, ProjectGitCommitSummary,
    ProjectGitDiffFile, ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind,
    ProjectGitDiffPayload, ProjectGitFileStatus, ProjectGitStatusPayload, ProjectId, ProjectPath,
    ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRootGitStatus, ProjectRootListing,
    ProjectRootPath, ProjectSearchFileResult, ProjectSearchMatch, ProjectSearchPayload,
    ReviewSummary, StreamPath,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};

use crate::review::ReviewRegistryHandle;
use crate::store::project::ProjectStore;
use crate::stream::Stream;

const PROJECT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);
const GIT_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const RECENT_HISTORY_LIMIT: usize = 100;
const BINARY_PREVIEW_LIMIT_BYTES: u64 = 32 * 1024 * 1024;
const CONTENT_SNIFF_BYTES: u64 = 8192;

struct ProjectWatcherFailure {
    message: String,
    limit_reached: bool,
}

impl ProjectWatcherFailure {
    fn from_notify(context: String, error: notify::Error) -> Self {
        Self {
            message: format!("{context}: {error}"),
            limit_reached: matches!(error.kind, notify::ErrorKind::MaxFilesWatch),
        }
    }

    fn forced_limit(root: &ProjectRootPath) -> Self {
        Self {
            message: format!(
                "failed to watch project root '{}': OS file watch limit reached.",
                root
            ),
            limit_reached: true,
        }
    }
}

struct ProjectWatcher {
    inner: Option<RecommendedWatcher>,
}

impl ProjectWatcher {
    fn new(inner: RecommendedWatcher) -> Self {
        Self { inner: Some(inner) }
    }
}

impl Drop for ProjectWatcher {
    fn drop(&mut self) {
        let Some(watcher) = self.inner.take() else {
            return;
        };
        match std::thread::Builder::new()
            .name("tyde-project-watch-drop".to_owned())
            .spawn(move || drop(watcher))
        {
            Ok(_) => {}
            Err(error) => tracing::warn!(
                %error,
                "failed to spawn project watcher drop thread; dropping watcher inline"
            ),
        }
    }
}

/// A (relative_path, kind) pair used for comparing file listings between snapshots.
pub(crate) type RawFileEntry = (String, ProjectFileKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitAccessMode {
    ReadOnly,
    Mutating,
}

#[derive(Debug)]
pub(crate) struct ProjectSnapshotState {
    /// Previous file entries per root, used to decide whether a new full snapshot is needed.
    pub file_entries: BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
    pub git_status: Option<Value>,
    pub diff_context_modes: HashMap<(StreamPath, ProjectDiffRequestKey), DiffContextMode>,
    pub code_intel_overview: CodeIntelOverviewPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProjectDiffRequestKey {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub revision: ProjectDiffRevision,
    pub path: Option<String>,
}

pub(crate) struct ProjectStreamSubscription {
    pub task: JoinHandle<()>,
    pub handle: ProjectStreamHandle,
}

/// A single advance of the centralized per-file version counter, broadcast to
/// registered listeners (the code-intel services, §M4). This is an internal
/// server type — it never appears on the wire. It tells a listener "the file at
/// `path` is now at `version`", so a service that has this file subscribed can
/// re-read it, push `textDocument/didChange`, and re-resolve the semantic model
/// at the new version without waiting for the client to re-subscribe.
#[derive(Debug, Clone)]
pub(crate) struct FileVersionChange {
    pub path: ProjectPath,
    pub version: ProjectFileVersion,
}

/// Whether a connection tracks project files. Desktop renders a file browser
/// and code intelligence, so it needs the bootstrap listing plus every
/// refresh and change event. Mobile has no file browser: it discarded
/// `ProjectFileList`, `CodeIntelOverview` and `FilesChanged` on arrival while
/// the listing alone ran ~150 KB per project (1211 files in Tyde), which
/// dominated the mobile connect payload and starved the transport's
/// receiver-credit window until it killed the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectFileDelivery {
    /// Bootstrap listing, refresh fan-outs, code intel, and change events.
    Full,
    /// None of them. The bootstrap still carries project metadata, git status
    /// and review summaries, which mobile does render.
    Off,
}

impl ProjectFileDelivery {
    /// Frames that exist only to drive a file browser or code intelligence.
    fn wants_frame(self, kind: FrameKind) -> bool {
        match self {
            Self::Full => true,
            Self::Off => !matches!(
                kind,
                FrameKind::ProjectFileList | FrameKind::CodeIntelOverview
            ),
        }
    }
}

struct ProjectSubscriber {
    stream: Stream,
    file_delivery: ProjectFileDelivery,
}

#[derive(Clone)]
pub(crate) struct ProjectStreamHandle {
    tx: mpsc::UnboundedSender<ProjectStreamCommand>,
}

enum ProjectStreamCommand {
    AddSubscriber {
        host_path: StreamPath,
        stream: Stream,
        review_summaries: Vec<ReviewSummary>,
        file_delivery: ProjectFileDelivery,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveSubscriber {
        host_path: StreamPath,
    },
    Refresh {
        reply: oneshot::Sender<Result<(), String>>,
    },
    RememberDiffContext {
        host_path: StreamPath,
        key: ProjectDiffRequestKey,
        context_mode: DiffContextMode,
        reply: oneshot::Sender<Result<(), String>>,
    },
    EmitProjectEvent {
        payload: ProjectEventPayload,
        reply: oneshot::Sender<Result<(), String>>,
    },
    UpdateCodeIntelProviderStatus {
        root: ProjectRootPath,
        status: CodeIntelProviderStatus,
    },
    /// Read a file's contents, assigning it the next value of the centralized
    /// per-file version counter. This is the single bump point for the *read*
    /// source; watcher changes and agent writes bump the same counter.
    ReadFile {
        payload: ProjectReadFilePayload,
        reply: oneshot::Sender<Result<ProjectFileContentsPayload, String>>,
    },
    /// Peek the current version of a file without bumping it. Used after a
    /// code-intel service has already registered its version listener. Validates
    /// the path/root against the project first (same check as the read path), so
    /// a bad/unknown root never produces a version.
    CurrentFileVersion {
        path: ProjectPath,
        reply: oneshot::Sender<Result<ProjectFileVersion, String>>,
    },
    /// Validate a path/root against the project, reusing the read path's
    /// validation. Code-intel routing calls this before delegating to a
    /// provider so a service is never spawned for a root that isn't a real
    /// project root.
    ValidatePath {
        path: ProjectPath,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Register a listener for centralized per-file version bumps (§M4) and, in
    /// the same serialized actor turn, return the current file version *after*
    /// that listener is in the broadcast set. A watcher bump before this command
    /// is reflected in the returned version; a bump after it is queued to the new
    /// listener. There is no gap where a bump can be missed.
    RegisterFileVersionListenerAndCurrentVersion {
        path: ProjectPath,
        listener: mpsc::UnboundedSender<FileVersionChange>,
        reply: oneshot::Sender<Result<ProjectFileVersion, String>>,
    },
}

#[derive(Default)]
struct PendingProjectUpdate {
    files: bool,
    git: bool,
}

impl PendingProjectUpdate {
    fn is_empty(&self) -> bool {
        !self.files && !self.git
    }

    fn merge(&mut self, other: Self) {
        self.files |= other.files;
        self.git |= other.git;
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl ProjectStreamHandle {
    pub(crate) async fn add_subscriber(
        &self,
        host_path: StreamPath,
        stream: Stream,
        review_summaries: Vec<ReviewSummary>,
        file_delivery: ProjectFileDelivery,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::AddSubscriber {
                host_path,
                stream,
                review_summaries,
                file_delivery,
                reply,
            })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) async fn remove_subscriber(&self, host_path: StreamPath) {
        let _ = self
            .tx
            .send(ProjectStreamCommand::RemoveSubscriber { host_path });
    }

    pub(crate) async fn refresh(&self) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::Refresh { reply })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) async fn remember_diff_context_mode(
        &self,
        host_path: StreamPath,
        key: ProjectDiffRequestKey,
        context_mode: DiffContextMode,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::RememberDiffContext {
                host_path,
                key,
                context_mode,
                reply,
            })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) async fn emit_project_event(
        &self,
        payload: ProjectEventPayload,
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::EmitProjectEvent { payload, reply })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) fn update_code_intel_provider_status(
        &self,
        root: ProjectRootPath,
        status: CodeIntelProviderStatus,
    ) -> Result<(), String> {
        self.tx
            .send(ProjectStreamCommand::UpdateCodeIntelProviderStatus { root, status })
            .map_err(|_| "project stream subscription stopped".to_owned())
    }

    pub(crate) async fn read_file(
        &self,
        payload: ProjectReadFilePayload,
    ) -> Result<ProjectFileContentsPayload, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::ReadFile { payload, reply })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) async fn current_file_version(
        &self,
        path: ProjectPath,
    ) -> Result<ProjectFileVersion, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::CurrentFileVersion { path, reply })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) async fn validate_path(&self, path: ProjectPath) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::ValidatePath { path, reply })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    /// Register a listener for per-file version bumps (§M4) and return the
    /// current file version after that registration has been serialized through
    /// the project-stream actor. This is the only safe subscribe-time primitive:
    /// a separate "peek current version" followed later by listener registration
    /// can miss a watcher bump in between.
    pub(crate) async fn register_file_version_listener_and_current_version(
        &self,
        path: ProjectPath,
        listener: mpsc::UnboundedSender<FileVersionChange>,
    ) -> Result<ProjectFileVersion, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(
                ProjectStreamCommand::RegisterFileVersionListenerAndCurrentVersion {
                    path,
                    listener,
                    reply,
                },
            )
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }
}

pub(crate) async fn spawn_project_subscription(
    project_store: Arc<Mutex<ProjectStore>>,
    project_id: ProjectId,
    review_registry: ReviewRegistryHandle,
    force_watch_limit: bool,
) -> Result<ProjectStreamSubscription, String> {
    let project = load_subscription_project(&project_store, &project_id).await?;
    let (watch_tx, watch_rx) = mpsc::unbounded_channel();
    let watched_roots = project.root_paths();
    let snapshot = initialize_snapshot(&project)?;
    let (watcher_ready_tx, watcher_ready_rx) = mpsc::unbounded_channel();
    {
        let project = project.clone();
        let watch_tx = watch_tx.clone();
        let tx = watcher_ready_tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("tyde-project-watch-init".to_owned())
            .spawn(move || {
                let result = create_project_watcher(&project, watch_tx, force_watch_limit);
                let _ = tx.send(result);
            })
        {
            let _ = watcher_ready_tx.send(Err(ProjectWatcherFailure {
                message: format!("failed to spawn project watcher initialization thread: {error}"),
                limit_reached: false,
            }));
        }
    }
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let handle = ProjectStreamHandle { tx: command_tx };

    let task = tokio::spawn(async move {
        run_project_subscription(
            project_store,
            project_id,
            project,
            snapshot,
            None,
            watched_roots,
            watch_tx,
            watch_rx,
            watcher_ready_rx,
            command_rx,
            review_registry,
        )
        .await;
    });

    Ok(ProjectStreamSubscription { task, handle })
}

async fn load_subscription_project(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
) -> Result<Project, String> {
    let projects = project_store
        .lock()
        .await
        .list()
        .map_err(|error| error.to_string())?;
    projects
        .into_iter()
        .find(|project| &project.id == project_id)
        .ok_or_else(|| format!("project {} disappeared while stream was active", project_id))
}

fn create_project_watcher(
    project: &Project,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    force_watch_limit: bool,
) -> Result<ProjectWatcher, ProjectWatcherFailure> {
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = watch_tx.send(result);
        },
        Config::default(),
    )
    .map_err(|error| {
        ProjectWatcherFailure::from_notify(
            "failed to create project filesystem watcher".to_owned(),
            error,
        )
    })?;

    for root in project.root_paths() {
        if force_watch_limit {
            return Err(ProjectWatcherFailure::forced_limit(&root));
        }
        watcher
            .watch(Path::new(&root.0), RecursiveMode::Recursive)
            .map_err(|error| {
                ProjectWatcherFailure::from_notify(
                    format!("failed to watch project root '{}'", root),
                    error,
                )
            })?;
    }

    Ok(ProjectWatcher::new(watcher))
}

fn initialize_snapshot(project: &Project) -> Result<ProjectSnapshotState, String> {
    let mut snapshot = ProjectSnapshotState {
        file_entries: scan_raw_entries(project)?,
        git_status: None,
        diff_context_modes: HashMap::new(),
        code_intel_overview: initial_code_intel_overview(project.root_paths()),
    };
    let git_status = build_git_status(project)?;
    snapshot.git_status = Some(serialize_git_status(&git_status)?);
    Ok(snapshot)
}

#[allow(clippy::too_many_arguments)]
async fn run_project_subscription(
    project_store: Arc<Mutex<ProjectStore>>,
    project_id: ProjectId,
    mut project: Project,
    mut snapshot: ProjectSnapshotState,
    mut watcher: Option<ProjectWatcher>,
    mut watched_roots: Vec<ProjectRootPath>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    mut watch_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    mut watcher_ready_rx: mpsc::UnboundedReceiver<Result<ProjectWatcher, ProjectWatcherFailure>>,
    mut command_rx: mpsc::UnboundedReceiver<ProjectStreamCommand>,
    review_registry: ReviewRegistryHandle,
) {
    let mut subscribers = HashMap::<StreamPath, ProjectSubscriber>::new();
    // Centralized per-file version counter. The single bump point
    // (`bump_file_version`) is funneled here from every source: file reads,
    // filesystem-watcher changes, and (transitively, via the watcher) agent
    // writes. See `dev-docs/24-code-intelligence.md` §2.4.
    let mut file_versions = HashMap::<ProjectPath, ProjectFileVersion>::new();
    // Registered listeners for per-file version bumps (§M4). Each per-root
    // code-intel service registers one on spawn; closed senders are pruned on
    // broadcast. The project-stream actor is the single owner of the counter,
    // so this is the one fan-out point for "a watched file's version changed."
    let mut file_version_listeners = Vec::<mpsc::UnboundedSender<FileVersionChange>>::new();
    let mut pending_file_version_changes = HashMap::<ProjectPath, ProjectFileVersion>::new();
    // Watcher-event → ProjectPath resolver, matching canonicalized roots too
    // (macOS FSEvents reports symlink-resolved paths). Re-synced per event so a
    // root add/remove picked up by a refresh is reflected without re-canonicalizing
    // on every event.
    let mut watcher_roots = WatcherRootPaths::new(project.root_paths());
    let mut pending_update = PendingProjectUpdate::default();
    let mut watcher_initializing = watcher.is_none();
    let mut watcher_warning = None::<String>;
    let mut debounce_active = false;
    let mut debounce_sleep = Box::pin(sleep(Duration::from_secs(60 * 60 * 24 * 365)));
    let mut git_poll = interval_at(
        Instant::now() + GIT_STATUS_POLL_INTERVAL,
        GIT_STATUS_POLL_INTERVAL,
    );
    git_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    return;
                };
                match command {
                    ProjectStreamCommand::AddSubscriber { host_path, stream, review_summaries, file_delivery, reply } => {
                        use std::collections::hash_map::Entry;
                        match subscribers.entry(host_path.clone()) {
                            Entry::Occupied(mut e) => {
                                e.insert(ProjectSubscriber { stream, file_delivery });
                                let _ = reply.send(Ok(()));
                                continue;
                            }
                            Entry::Vacant(e) => {
                                e.insert(ProjectSubscriber { stream: stream.clone(), file_delivery });
                            }
                        }
                        let result = emit_snapshot_to_stream(&stream, &project, &snapshot, review_summaries, file_delivery).await;
                        if result.is_err() {
                            subscribers.remove(&host_path);
                            snapshot.diff_context_modes.retain(|(subscriber, _), _| subscriber != &host_path);
                        } else if let Some(message) = watcher_warning.as_ref() {
                            emit_project_command_error(
                                &stream,
                                FrameKind::ProjectFileList,
                                "project_watch",
                                message.clone(),
                                false,
                            ).await;
                        }
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::RemoveSubscriber { host_path } => {
                        subscribers.remove(&host_path);
                        snapshot.diff_context_modes.retain(|(subscriber, _), _| subscriber != &host_path);
                    }
                    ProjectStreamCommand::Refresh { reply } => {
                        let result = if let Some(watcher) = watcher.as_mut() {
                            refresh_full(
                                &project_store,
                                &project_id,
                                &mut project,
                                &mut snapshot,
                                watcher,
                                &mut watched_roots,
                                watch_tx.clone(),
                                &mut subscribers,
                                &review_registry,
                            ).await
                        } else {
                            refresh_full_unwatched(
                                &project_store,
                                &project_id,
                                &mut project,
                                &mut snapshot,
                                &mut subscribers,
                                &review_registry,
                            ).await
                        };
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::RememberDiffContext { host_path, key, context_mode, reply } => {
                        snapshot.diff_context_modes.insert((host_path, key), context_mode);
                        let _ = reply.send(Ok(()));
                    }
                    ProjectStreamCommand::EmitProjectEvent { payload, reply } => {
                        let result = broadcast_project_event(&mut subscribers, &payload).await;
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::UpdateCodeIntelProviderStatus { root, status } => {
                        let provider = status.provider.clone();
                        let language = status.language.clone();
                        let state = status.state;
                        let resource_mode = status.resource_mode;
                        if update_code_intel_provider_status(
                            &mut snapshot.code_intel_overview,
                            &project,
                            root.clone(),
                            status,
                        ) && let Err(error) = fan_out_payload(
                            &mut subscribers,
                            FrameKind::CodeIntelOverview,
                            &snapshot.code_intel_overview,
                        )
                        .await
                        {
                            tracing::error!(
                                project_id = %project_id,
                                root = %root,
                                provider = %provider,
                                language = %language,
                                ?state,
                                ?resource_mode,
                                error = %error,
                                "failed to fan out code-intel overview update"
                            );
                        }
                    }
                    ProjectStreamCommand::ReadFile { payload, reply } => {
                        // A read never changes the file, so it must NOT advance
                        // the version. Stamp the contents with the *current*
                        // centralized version (peek) — the value the last real
                        // change (watcher event / write) produced. Bumping here
                        // would inflate the counter far past the number of
                        // actual edits (every panel read, agent Read tool, etc.
                        // counted as a "change"), so the version a watcher event
                        // later broadcasts to the code-intel provider would jump
                        // arbitrarily high while the client's rendered version
                        // — set from this reply — stayed behind, and every
                        // code-intel query would be rejected as stale.
                        let result = read_file(&project, payload).map(|mut contents| {
                            contents.version = file_versions
                                .get(&contents.path)
                                .copied()
                                .unwrap_or(ProjectFileVersion(0));
                            contents
                        });
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::CurrentFileVersion { path, reply } => {
                        let result = current_file_version(&project, &path, &file_versions);
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::ValidatePath { path, reply } => {
                        let _ = reply.send(validate_project_path(&project, &path));
                    }
                    ProjectStreamCommand::RegisterFileVersionListenerAndCurrentVersion {
                        path,
                        listener,
                        reply,
                    } => {
                        let result = register_file_version_listener_and_current_version(
                            &project,
                            path,
                            &file_versions,
                            &mut file_version_listeners,
                            listener,
                        );
                        let _ = reply.send(result);
                    }
                }
            }
            maybe_watcher = watcher_ready_rx.recv(), if watcher_initializing => {
                watcher_initializing = false;
                match maybe_watcher {
                    Some(Ok(ready_watcher)) => {
                        watcher = Some(ready_watcher);
                        if let Some(watcher) = watcher.as_mut()
                            && let Err(error) = refresh_full(
                                &project_store,
                                &project_id,
                                &mut project,
                                &mut snapshot,
                                watcher,
                                &mut watched_roots,
                                watch_tx.clone(),
                                &mut subscribers,
                                &review_registry,
                            ).await
                        {
                            tracing::warn!(project_id = %project_id, error = %error, "stopping project subscription after watcher initialization refresh failure");
                            emit_fatal_project_stream_error(&mut subscribers, "project_watch", error).await;
                            return;
                        }
                    }
                    Some(Err(error)) if error.limit_reached => {
                        let message = project_watch_limit_guidance(&error.message);
                        tracing::warn!(project_id = %project_id, error = %message, "continuing project subscription without filesystem watching");
                        emit_project_stream_warning(&mut subscribers, "project_watch", message.clone()).await;
                        watcher_warning = Some(message);
                    }
                    Some(Err(error)) => {
                        tracing::warn!(project_id = %project_id, error = %error.message, "stopping project subscription after watcher initialization failure");
                        emit_fatal_project_stream_error(&mut subscribers, "project_watch", error.message).await;
                        return;
                    }
                    None => {
                        tracing::warn!(project_id = %project_id, "stopping project subscription after watcher initialization channel closed");
                        emit_fatal_project_stream_error(
                            &mut subscribers,
                            "project_watch",
                            "project filesystem watcher failed to initialize".to_owned(),
                        ).await;
                        return;
                    }
                }
            }
            maybe_event = watch_rx.recv(), if watcher.is_some() => {
                let Some(event_result) = maybe_event else {
                    emit_fatal_project_stream_error(
                        &mut subscribers,
                        "project_watch",
                        "project filesystem watcher stopped unexpectedly".to_owned(),
                    ).await;
                    return;
                };

                match event_result {
                    Ok(event) => {
                        // Bump the centralized per-file version for each changed
                        // file through the single bump point (`bump_file_version`),
                        // the same one reads use. An external change, a branch
                        // switch, or an agent write all land on disk and reach us
                        // here via the watcher, so this one point advances the
                        // version the code-intel service resolves against —
                        // regardless of which source caused the change.
                        //
                        // §M4: each bumped (path, version) is then broadcast to
                        // every registered code-intel service so it re-reads,
                        // sends `didChange`, and re-pushes the semantic model at
                        // the new version, superseding any stale in-flight
                        // resolution. The counter stays single and authoritative;
                        // there is no second counter and no double-bump.
                        watcher_roots.sync(project.root_paths());
                        let changes = bump_watched_paths(&watcher_roots, &event, &mut file_versions);
                        merge_file_version_changes(&mut pending_file_version_changes, changes);
                        let refresh = classify_watch_event(&event);
                        if !refresh.is_empty() || !pending_file_version_changes.is_empty() {
                            pending_update.merge(refresh);
                            debounce_active = true;
                            debounce_sleep.as_mut().reset(Instant::now() + PROJECT_REFRESH_DEBOUNCE);
                        }
                    }
                    Err(error) if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) => {
                        let message = project_watch_limit_guidance(&format!("project filesystem watcher failed: {error}"));
                        tracing::warn!(project_id = %project_id, error = %message, "continuing project subscription without filesystem watching");
                        watcher = None;
                        emit_project_stream_warning(&mut subscribers, "project_watch", message.clone()).await;
                        watcher_warning = Some(message);
                    }
                    Err(error) => {
                        let message = format!("project filesystem watcher failed: {error}");
                        tracing::warn!(project_id = %project_id, error = %message, "stopping project subscription");
                        emit_fatal_project_stream_error(&mut subscribers, "project_watch", message).await;
                        return;
                    }
                }
            }
            _ = &mut debounce_sleep, if debounce_active => {
                debounce_active = false;
                let refresh = pending_update.take();
                let result = if let Some(watcher) = watcher.as_mut() {
                    refresh_incremental(
                        &project_store,
                        &project_id,
                        &mut project,
                        &mut snapshot,
                        watcher,
                        &mut watched_roots,
                        watch_tx.clone(),
                        &mut subscribers,
                        &review_registry,
                        refresh.files,
                        refresh.git,
                    ).await
                } else {
                    refresh_full_unwatched(
                        &project_store,
                        &project_id,
                        &mut project,
                        &mut snapshot,
                        &mut subscribers,
                        &review_registry,
                    ).await
                };
                if let Err(error) = result {
                    tracing::warn!(project_id = %project_id, error = %error, "stopping project subscription after debounced refresh failure");
                    emit_fatal_project_stream_error(&mut subscribers, "project_watch", error).await;
                    return;
                }
                let changes = take_pending_file_version_changes(&mut pending_file_version_changes);
                notify_file_version_listeners(&mut file_version_listeners, &changes);
                // §M4: also tell the *frontend* which files advanced, so it can
                // re-read any it has open and keep its rendered version (and the
                // version it stamps on code-intel queries) in step with the
                // server. The internal listener broadcast above only reaches the
                // code-intel services; nothing else re-syncs the client's copy.
                if !changes.is_empty() {
                    let files = changes
                        .iter()
                        .map(|change| ProjectFileVersionChange {
                            path: change.path.clone(),
                            version: change.version,
                        })
                        .collect();
                    if let Err(error) = broadcast_project_event(
                        &mut subscribers,
                        &ProjectEventPayload::FilesChanged { files },
                    )
                    .await
                    {
                        tracing::warn!(
                            project_id = %project_id,
                            error = %error,
                            "failed to broadcast file-version changes to subscribers"
                        );
                    }
                }
            }
            _ = git_poll.tick() => {
                let result = if let Some(watcher) = watcher.as_mut() {
                    refresh_incremental(
                        &project_store,
                        &project_id,
                        &mut project,
                        &mut snapshot,
                        watcher,
                        &mut watched_roots,
                        watch_tx.clone(),
                        &mut subscribers,
                        &review_registry,
                        false,
                        true,
                    ).await
                } else {
                    refresh_git_status_unwatched(
                        &project_store,
                        &project_id,
                        &mut project,
                        &mut snapshot,
                        &mut subscribers,
                        &review_registry,
                    ).await
                };
                if let Err(error) = result {
                    tracing::warn!(project_id = %project_id, error = %error, "stopping project subscription after git status refresh failure");
                    emit_fatal_project_stream_error(&mut subscribers, "project_git_status", error).await;
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn refresh_full(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    project: &mut Project,
    snapshot: &mut ProjectSnapshotState,
    watcher: &mut ProjectWatcher,
    watched_roots: &mut Vec<ProjectRootPath>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    review_registry: &ReviewRegistryHandle,
) -> Result<(), String> {
    let latest_project = load_subscription_project(project_store, project_id).await?;
    ensure_watched_roots(&latest_project, watcher, watched_roots, watch_tx)?;

    let raw_entries = scan_raw_entries(&latest_project)?;
    let file_list = full_file_list_from_raw(&latest_project, &raw_entries);
    let git_status = build_git_status(&latest_project)?;
    let git_json = serialize_git_status(&git_status)?;

    *project = latest_project;
    snapshot.file_entries = raw_entries;
    snapshot.git_status = Some(git_json);
    let code_intel_roots_changed =
        sync_code_intel_overview_roots(&mut snapshot.code_intel_overview, project.root_paths());

    fan_out_payload(subscribers, FrameKind::ProjectFileList, &file_list).await?;
    fan_out_payload(subscribers, FrameKind::ProjectGitStatus, &git_status).await?;
    if code_intel_roots_changed {
        fan_out_payload(
            subscribers,
            FrameKind::CodeIntelOverview,
            &snapshot.code_intel_overview,
        )
        .await?;
    }
    reset_reviews_for_clean_unstaged_roots(review_registry, project_id, &git_status).await;
    refresh_remembered_diffs(project, snapshot, subscribers).await;
    Ok(())
}

async fn refresh_full_unwatched(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    project: &mut Project,
    snapshot: &mut ProjectSnapshotState,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    review_registry: &ReviewRegistryHandle,
) -> Result<(), String> {
    let latest_project = load_subscription_project(project_store, project_id).await?;
    let raw_entries = scan_raw_entries(&latest_project)?;
    let file_list = full_file_list_from_raw(&latest_project, &raw_entries);
    let git_status = build_git_status(&latest_project)?;
    let git_json = serialize_git_status(&git_status)?;

    *project = latest_project;
    snapshot.file_entries = raw_entries;
    snapshot.git_status = Some(git_json);
    let code_intel_roots_changed =
        sync_code_intel_overview_roots(&mut snapshot.code_intel_overview, project.root_paths());

    fan_out_payload(subscribers, FrameKind::ProjectFileList, &file_list).await?;
    fan_out_payload(subscribers, FrameKind::ProjectGitStatus, &git_status).await?;
    if code_intel_roots_changed {
        fan_out_payload(
            subscribers,
            FrameKind::CodeIntelOverview,
            &snapshot.code_intel_overview,
        )
        .await?;
    }
    reset_reviews_for_clean_unstaged_roots(review_registry, project_id, &git_status).await;
    refresh_remembered_diffs(project, snapshot, subscribers).await;
    Ok(())
}

async fn refresh_git_status_unwatched(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    project: &mut Project,
    snapshot: &mut ProjectSnapshotState,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    review_registry: &ReviewRegistryHandle,
) -> Result<(), String> {
    let latest_project = load_subscription_project(project_store, project_id).await?;
    let git_status = build_git_status(&latest_project)?;
    let git_json = serialize_git_status(&git_status)?;
    let git_changed = snapshot.git_status.as_ref() != Some(&git_json);

    *project = latest_project;
    let code_intel_roots_changed =
        sync_code_intel_overview_roots(&mut snapshot.code_intel_overview, project.root_paths());

    if git_changed {
        snapshot.git_status = Some(git_json);
        fan_out_payload(subscribers, FrameKind::ProjectGitStatus, &git_status).await?;
        reset_reviews_for_clean_unstaged_roots(review_registry, project_id, &git_status).await;
        refresh_remembered_diffs(project, snapshot, subscribers).await;
    }
    if code_intel_roots_changed {
        fan_out_payload(
            subscribers,
            FrameKind::CodeIntelOverview,
            &snapshot.code_intel_overview,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn refresh_incremental(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    project: &mut Project,
    snapshot: &mut ProjectSnapshotState,
    watcher: &mut ProjectWatcher,
    watched_roots: &mut Vec<ProjectRootPath>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    review_registry: &ReviewRegistryHandle,
    files_changed: bool,
    git_changed: bool,
) -> Result<(), String> {
    if !files_changed && !git_changed {
        return Ok(());
    }

    let latest_project = load_subscription_project(project_store, project_id).await?;
    ensure_watched_roots(&latest_project, watcher, watched_roots, watch_tx)?;
    *project = latest_project;
    let code_intel_roots_changed =
        sync_code_intel_overview_roots(&mut snapshot.code_intel_overview, project.root_paths());

    if files_changed {
        let current_raw = scan_raw_entries(project)?;
        if snapshot.file_entries != current_raw {
            snapshot.file_entries = current_raw;
            let file_list = full_file_list_from_raw(project, &snapshot.file_entries);
            fan_out_payload(subscribers, FrameKind::ProjectFileList, &file_list).await?;
        }
    }

    if git_changed {
        let git_status = build_git_status(project)?;
        let git_json = serialize_git_status(&git_status)?;
        if snapshot.git_status.as_ref() != Some(&git_json) {
            snapshot.git_status = Some(git_json);
            fan_out_payload(subscribers, FrameKind::ProjectGitStatus, &git_status).await?;
            reset_reviews_for_clean_unstaged_roots(review_registry, project_id, &git_status).await;
            refresh_remembered_diffs(project, snapshot, subscribers).await;
        }
    }

    if code_intel_roots_changed {
        fan_out_payload(
            subscribers,
            FrameKind::CodeIntelOverview,
            &snapshot.code_intel_overview,
        )
        .await?;
    }

    Ok(())
}

fn ensure_watched_roots(
    project: &Project,
    watcher: &mut ProjectWatcher,
    watched_roots: &mut Vec<ProjectRootPath>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
) -> Result<(), String> {
    let roots = project.root_paths();
    if *watched_roots == roots {
        return Ok(());
    }

    *watcher = create_project_watcher(project, watch_tx, false).map_err(|error| error.message)?;
    *watched_roots = roots;
    Ok(())
}

fn initial_code_intel_overview(roots: Vec<ProjectRootPath>) -> CodeIntelOverviewPayload {
    let roots = roots
        .into_iter()
        .map(|root| CodeIntelRootOverview {
            root,
            providers: Vec::new(),
        })
        .collect::<Vec<_>>();
    code_intel_overview_with_summary(roots)
}

fn code_intel_overview_with_summary(roots: Vec<CodeIntelRootOverview>) -> CodeIntelOverviewPayload {
    let summary = summarize_code_intel_overview(&roots);
    CodeIntelOverviewPayload { roots, summary }
}

fn sync_code_intel_overview_roots(
    overview: &mut CodeIntelOverviewPayload,
    roots: Vec<ProjectRootPath>,
) -> bool {
    let previous = overview.clone();
    let mut synced = Vec::with_capacity(roots.len());
    for root in roots {
        let providers = overview
            .roots
            .iter()
            .find(|entry| entry.root == root)
            .map(|entry| entry.providers.clone())
            .unwrap_or_default();
        synced.push(CodeIntelRootOverview { root, providers });
    }
    *overview = code_intel_overview_with_summary(synced);
    *overview != previous
}

fn update_code_intel_provider_status(
    overview: &mut CodeIntelOverviewPayload,
    project: &Project,
    root: ProjectRootPath,
    status: CodeIntelProviderStatus,
) -> bool {
    if !project.root_paths().contains(&root) {
        tracing::debug!(
            root = %root,
            provider = %status.provider,
            language = %status.language,
            "ignoring code-intel status for root no longer in project"
        );
        return false;
    }

    sync_code_intel_overview_roots(overview, project.root_paths());
    let previous = overview.clone();
    let Some(root_entry) = overview.roots.iter_mut().find(|entry| entry.root == root) else {
        return false;
    };
    match root_entry.providers.iter_mut().find(|provider| {
        provider.provider == status.provider && provider.language == status.language
    }) {
        Some(existing) => *existing = status,
        None => root_entry.providers.push(status),
    }
    root_entry.providers.sort_by(|left, right| {
        provider_status_sort_key(left).cmp(&provider_status_sort_key(right))
    });
    overview.summary = summarize_code_intel_overview(&overview.roots);
    *overview != previous
}

fn provider_status_sort_key(status: &CodeIntelProviderStatus) -> (&str, &str) {
    (status.provider.0.as_str(), status.language.0.as_str())
}

fn summarize_code_intel_overview(roots: &[CodeIntelRootOverview]) -> CodeIntelOverviewSummary {
    let mut ready = 0;
    let mut indexing = 0;
    let mut starting = 0;
    let mut unavailable = 0;
    let mut failed = 0;
    let mut error_count = 0;
    let mut warning_count = 0;
    for provider in roots.iter().flat_map(|root| root.providers.iter()) {
        match provider.state {
            CodeIntelState::Ready => ready += 1,
            CodeIntelState::Indexing => indexing += 1,
            CodeIntelState::Starting => starting += 1,
            CodeIntelState::Unavailable => unavailable += 1,
            CodeIntelState::Failed => failed += 1,
            CodeIntelState::Unsupported => {}
        }
        error_count += provider.error_count;
        warning_count += provider.warning_count;
    }

    let provider_count = ready + indexing + starting + unavailable + failed;
    let headline = if provider_count == 0 {
        CodeIntelOverviewHeadline::NotStarted
    } else if failed > 0 {
        CodeIntelOverviewHeadline::Failed
    } else if unavailable > 0 {
        CodeIntelOverviewHeadline::Unavailable
    } else if indexing > 0 {
        CodeIntelOverviewHeadline::Indexing
    } else if starting > 0 {
        CodeIntelOverviewHeadline::Starting
    } else {
        CodeIntelOverviewHeadline::Ready
    };
    // For terminal headlines, prefer the provider's own message — for a
    // missing binary it carries the actionable install hint ("pyright not
    // installed — run `npm install -g pyright`") that the generic label lacks.
    let provider_message_for = |target: CodeIntelState| {
        roots
            .iter()
            .flat_map(|root| root.providers.iter())
            .find(|provider| provider.state == target)
            .and_then(|provider| provider.message.clone())
    };
    let message = match headline {
        CodeIntelOverviewHeadline::NotStarted => Some(
            "No language server running — select the project or launch an agent to index"
                .to_owned(),
        ),
        CodeIntelOverviewHeadline::Failed => Some(
            provider_message_for(CodeIntelState::Failed)
                .unwrap_or_else(|| "Code intelligence failed".to_owned()),
        ),
        CodeIntelOverviewHeadline::Unavailable => Some(
            provider_message_for(CodeIntelState::Unavailable)
                .unwrap_or_else(|| "Language server unavailable".to_owned()),
        ),
        CodeIntelOverviewHeadline::Indexing => Some("Indexing code intelligence".to_owned()),
        CodeIntelOverviewHeadline::Starting => Some("Starting language server".to_owned()),
        CodeIntelOverviewHeadline::Ready => Some("Code intelligence ready".to_owned()),
    };

    CodeIntelOverviewSummary {
        headline,
        ready,
        indexing,
        starting,
        unavailable,
        failed,
        message,
        error_count,
        warning_count,
    }
}

fn current_file_version(
    project: &Project,
    path: &ProjectPath,
    versions: &HashMap<ProjectPath, ProjectFileVersion>,
) -> Result<ProjectFileVersion, String> {
    validate_project_path(project, path)
        .map(|()| versions.get(path).copied().unwrap_or(ProjectFileVersion(0)))
}

trait FileVersionLookup {
    fn version_for(&self, path: &ProjectPath) -> Option<ProjectFileVersion>;
}

impl FileVersionLookup for HashMap<ProjectPath, ProjectFileVersion> {
    fn version_for(&self, path: &ProjectPath) -> Option<ProjectFileVersion> {
        self.get(path).copied()
    }
}

trait FileVersionListenerRegistry {
    fn register(&mut self, listener: mpsc::UnboundedSender<FileVersionChange>);
}

impl FileVersionListenerRegistry for Vec<mpsc::UnboundedSender<FileVersionChange>> {
    fn register(&mut self, listener: mpsc::UnboundedSender<FileVersionChange>) {
        self.push(listener);
    }
}

fn register_file_version_listener_and_current_version<V, L>(
    project: &Project,
    path: ProjectPath,
    versions: &V,
    listeners: &mut L,
    listener: mpsc::UnboundedSender<FileVersionChange>,
) -> Result<ProjectFileVersion, String>
where
    V: FileVersionLookup,
    L: FileVersionListenerRegistry,
{
    validate_project_path(project, &path)?;
    listeners.register(listener);
    Ok(versions.version_for(&path).unwrap_or(ProjectFileVersion(0)))
}

/// The single bump point for the centralized per-file version counter. Every
/// source (read, watcher change, agent write) funnels through here so a file's
/// version increases monotonically regardless of which path advanced it.
fn bump_file_version(
    versions: &mut HashMap<ProjectPath, ProjectFileVersion>,
    path: &ProjectPath,
) -> ProjectFileVersion {
    let slot = versions
        .entry(path.clone())
        .or_insert(ProjectFileVersion(0));
    slot.0 += 1;
    *slot
}

/// Resolves absolute filesystem-watcher event paths back to `ProjectPath`s.
///
/// Watcher backends do not necessarily report events under the path a root was
/// registered with: macOS FSEvents resolves symlinks first, so a root
/// configured as `/tmp/project` produces events under `/private/tmp/project`.
/// Matching only the configured root string silently drops every per-file
/// version bump for a root behind a symlink — code-intel `didChange` sync and
/// open-viewer reloads go dead while the path-agnostic file-tree refresh keeps
/// working (verified QA regression). So each root is matched by its configured
/// path *and* its canonicalized path. Canonicalization is IO, so it happens
/// once per root here — never on the per-event path.
struct WatcherRootPaths {
    roots: Vec<ProjectRootPath>,
    /// Per root, the path prefixes an event path is matched against: the
    /// configured path first, then (when different) its canonical form.
    prefixes: Vec<(ProjectRootPath, Vec<PathBuf>)>,
}

impl WatcherRootPaths {
    fn new(roots: Vec<ProjectRootPath>) -> Self {
        let prefixes = roots
            .iter()
            .map(|root| {
                let raw = PathBuf::from(&root.0);
                let mut candidates = vec![raw.clone()];
                match fs::canonicalize(&root.0) {
                    Ok(canonical) if canonical != raw => candidates.push(canonical),
                    Ok(_) => {}
                    Err(error) => tracing::debug!(
                        %error,
                        root = %root.0,
                        "watcher root canonicalization failed; matching the configured path only"
                    ),
                }
                (root.clone(), candidates)
            })
            .collect();
        Self { roots, prefixes }
    }

    /// Rebuild only when the project's root list changed. The comparison runs
    /// per watcher event (cheap: a few string compares); the canonicalization
    /// IO only reruns on an actual root change.
    fn sync(&mut self, roots: Vec<ProjectRootPath>) {
        if self.roots != roots {
            *self = Self::new(roots);
        }
    }

    fn project_path_for(&self, absolute: &Path) -> Option<ProjectPath> {
        if !absolute.is_absolute() {
            return None;
        }
        // Most-specific (longest) matching root wins: with nested roots
        // `/repo` and `/repo/sub`, an event under `/repo/sub` must be
        // attributed to the nested root — that is the root whose code-intel
        // service holds the subscription. First-match order would silently
        // route the bump to the outer root.
        let mut best: Option<(usize, Option<ProjectPath>)> = None;
        for (root, candidates) in &self.prefixes {
            for prefix in candidates {
                let Ok(relative) = absolute.strip_prefix(prefix) else {
                    continue;
                };
                let specificity = prefix.components().count();
                if best
                    .as_ref()
                    .is_some_and(|(existing, _)| *existing >= specificity)
                {
                    continue;
                }
                let relative_path = relative.to_string_lossy().replace('\\', "/");
                // An empty relative path means the event is for this root
                // directory itself, not a file inside it.
                let mapped = (!relative_path.is_empty()).then(|| ProjectPath {
                    root: root.clone(),
                    relative_path,
                });
                best = Some((specificity, mapped));
            }
        }
        best.and_then(|(_, mapped)| mapped)
    }
}

/// Map every changed path in a watch event back to a `ProjectPath` and bump its
/// version through the single bump point. Paths inside `.git` are ignored — git
/// bookkeeping is not a source file change. Returns one [`FileVersionChange`]
/// per bumped file (in event order) so the caller can broadcast them to the
/// code-intel services (§M4). Access and metadata-only events are not content
/// changes, and duplicate paths inside one notify event are coalesced so a
/// single filesystem change advances the counter once.
fn bump_watched_paths(
    watcher_roots: &WatcherRootPaths,
    event: &Event,
    versions: &mut HashMap<ProjectPath, ProjectFileVersion>,
) -> Vec<FileVersionChange> {
    if !watch_event_changes_contents(event) {
        return Vec::new();
    }
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    for path in &event.paths {
        if is_inside_git(path) {
            continue;
        }
        if let Some(project_path) = watcher_roots.project_path_for(path) {
            if !seen.insert(project_path.clone()) {
                continue;
            }
            let version = bump_file_version(versions, &project_path);
            changes.push(FileVersionChange {
                path: project_path,
                version,
            });
        }
    }
    changes
}

/// Broadcast each per-file version bump to every registered listener (the
/// code-intel services, §M4), pruning any whose receiver has been dropped. A
/// listener that fails on any change is closed and removed — it is the service
/// task having exited, so there is nothing left to notify.
fn notify_file_version_listeners(
    listeners: &mut Vec<mpsc::UnboundedSender<FileVersionChange>>,
    changes: &[FileVersionChange],
) {
    if listeners.is_empty() || changes.is_empty() {
        return;
    }
    listeners.retain(|listener| {
        changes
            .iter()
            .all(|change| listener.send(change.clone()).is_ok())
    });
}

fn merge_file_version_changes(
    pending: &mut HashMap<ProjectPath, ProjectFileVersion>,
    changes: Vec<FileVersionChange>,
) {
    for change in changes {
        pending.insert(change.path, change.version);
    }
}

fn take_pending_file_version_changes(
    pending: &mut HashMap<ProjectPath, ProjectFileVersion>,
) -> Vec<FileVersionChange> {
    let mut changes = pending
        .drain()
        .map(|(path, version)| FileVersionChange { path, version })
        .collect::<Vec<_>>();
    changes.sort_by(|left, right| {
        left.path
            .root
            .0
            .cmp(&right.path.root.0)
            .then_with(|| left.path.relative_path.cmp(&right.path.relative_path))
    });
    changes
}

fn classify_watch_event(event: &Event) -> PendingProjectUpdate {
    let mut refresh = PendingProjectUpdate::default();
    for path in &event.paths {
        if is_git_head_or_index(path) && watch_event_refreshes_git_metadata(event) {
            refresh.git = true;
        } else if !is_inside_git(path) && watch_event_changes_contents(event) {
            refresh.files = true;
            refresh.git = true;
        }
    }
    refresh
}

fn watch_event_changes_contents(event: &Event) -> bool {
    match event.kind {
        EventKind::Access(_) => false,
        EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => false,
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Other => true,
    }
}

fn watch_event_refreshes_git_metadata(event: &Event) -> bool {
    !matches!(event.kind, EventKind::Access(_))
}

fn is_inside_git(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(name) if name == std::ffi::OsStr::new(".git"))
    })
}

fn is_git_head_or_index(path: &Path) -> bool {
    let mut saw_git = false;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        if saw_git {
            return name == std::ffi::OsStr::new("HEAD") || name == std::ffi::OsStr::new("index");
        }
        saw_git = name == std::ffi::OsStr::new(".git");
    }
    false
}

fn full_file_list_from_raw(
    project: &Project,
    raw_entries: &BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
) -> ProjectFileListPayload {
    let roots = project
        .root_paths()
        .into_iter()
        .map(|root| {
            let entries = raw_entries
                .get(&root)
                .into_iter()
                .flat_map(|entries| entries.iter())
                .map(|(path, kind)| ProjectFileEntry {
                    relative_path: path.clone(),
                    kind: *kind,
                    op: FileEntryOp::Add,
                })
                .collect();
            ProjectRootListing { root, entries }
        })
        .collect();
    ProjectFileListPayload {
        incremental: false,
        roots,
    }
}

async fn reset_reviews_for_clean_unstaged_roots(
    review_registry: &ReviewRegistryHandle,
    project_id: &ProjectId,
    git_status: &ProjectGitStatusPayload,
) {
    let roots = git_status
        .roots
        .iter()
        .filter(|root| root_unstaged_changes_are_clean(root))
        .map(|root| root.root.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return;
    }
    if let Err(error) = review_registry
        .reset_project_roots_for_clean_unstaged(project_id.clone(), roots)
        .await
    {
        tracing::warn!(
            project_id = %project_id,
            error = %error,
            "failed to reset reviews after clean unstaged git status"
        );
    }
}

fn root_unstaged_changes_are_clean(root: &ProjectRootGitStatus) -> bool {
    root.files
        .iter()
        .all(|file| file.unstaged.is_none() && !file.untracked)
}

fn serialize_git_status(git_status: &ProjectGitStatusPayload) -> Result<Value, String> {
    serde_json::to_value(git_status)
        .map_err(|error| format!("failed to serialize project git status: {error}"))
}

async fn emit_snapshot_to_stream(
    stream: &Stream,
    project: &Project,
    snapshot: &ProjectSnapshotState,
    review_summaries: Vec<ReviewSummary>,
    file_delivery: ProjectFileDelivery,
) -> Result<(), String> {
    // Skipped rather than computed-then-discarded: walking every root of every
    // project is host work, not just wire bytes.
    let file_list = match file_delivery {
        ProjectFileDelivery::Full => full_file_list_from_raw(project, &snapshot.file_entries),
        ProjectFileDelivery::Off => protocol::ProjectFileListPayload {
            incremental: false,
            roots: Vec::new(),
        },
    };
    let Some(git_status) = snapshot.git_status.clone() else {
        return Err("project git status snapshot was not initialized".to_owned());
    };
    let git_status = serde_json::from_value(git_status)
        .map_err(|error| format!("failed to parse project git status snapshot: {error}"))?;
    let bootstrap = protocol::ProjectBootstrapPayload {
        project: project.clone(),
        file_list,
        git_status,
        review_summaries,
    };
    send_payload(stream, FrameKind::ProjectBootstrap, &bootstrap).await?;
    if !file_delivery.wants_frame(FrameKind::CodeIntelOverview) {
        return Ok(());
    }
    send_payload(
        stream,
        FrameKind::CodeIntelOverview,
        &snapshot.code_intel_overview,
    )
    .await
}

async fn fan_out_payload<T: serde::Serialize>(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("failed to serialize {kind} payload: {error}"))?;
    fan_out_value(subscribers, kind, payload).await;
    Ok(())
}

async fn fan_out_value(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    kind: FrameKind,
    payload: Value,
) {
    let mut dead = Vec::new();
    for (host_path, subscriber) in subscribers.iter() {
        if !subscriber.file_delivery.wants_frame(kind) {
            continue;
        }
        if subscriber.stream.send_value(kind, payload.clone()).is_err() {
            dead.push(host_path.clone());
        }
    }
    for host_path in dead {
        subscribers.remove(&host_path);
    }
}

async fn broadcast_project_event(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    payload: &ProjectEventPayload,
) -> Result<(), String> {
    // `FilesChanged` only exists so a client can re-read files it has open;
    // it is not distinguishable by frame kind, so it is filtered here rather
    // than in `ProjectFileDelivery::wants_frame`.
    let files_only = matches!(payload, ProjectEventPayload::FilesChanged { .. });
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("failed to serialize ProjectEvent payload: {error}"))?;
    let mut dead = Vec::new();
    for (host_path, subscriber) in subscribers.iter() {
        if files_only && subscriber.file_delivery == ProjectFileDelivery::Off {
            continue;
        }
        if subscriber
            .stream
            .send_value(FrameKind::ProjectEvent, payload.clone())
            .is_err()
        {
            dead.push(host_path.clone());
        }
    }
    for host_path in dead {
        subscribers.remove(&host_path);
    }
    Ok(())
}

async fn refresh_remembered_diffs(
    project: &Project,
    snapshot: &ProjectSnapshotState,
    subscribers: &HashMap<StreamPath, ProjectSubscriber>,
) {
    let mut remembered = snapshot
        .diff_context_modes
        .iter()
        .map(|((host_path, key), context_mode)| (host_path.clone(), key.clone(), *context_mode))
        .collect::<Vec<_>>();
    remembered.sort_by(|(host_a, key_a, _), (host_b, key_b, _)| {
        host_a
            .0
            .cmp(&host_b.0)
            .then_with(|| key_a.root.0.cmp(&key_b.root.0))
            .then_with(|| diff_scope_sort_key(key_a.scope).cmp(&diff_scope_sort_key(key_b.scope)))
            .then_with(|| key_a.path.cmp(&key_b.path))
    });

    for (host_path, key, context_mode) in remembered {
        let Some(stream) = subscribers.get(&host_path).map(|s| &s.stream) else {
            continue;
        };
        let payload = ProjectReadDiffPayload {
            request_id: None,
            root: key.root.clone(),
            scope: key.scope,
            revision: key.revision.clone(),
            path: key.path.clone(),
            context_mode,
        };
        match read_diff(project, payload) {
            Ok(diff) => {
                let _ = send_payload(stream, FrameKind::ProjectGitDiff, &diff).await;
            }
            Err(error) => {
                emit_project_command_error(
                    stream,
                    FrameKind::ProjectReadDiff,
                    "project_read_diff",
                    error,
                    false,
                )
                .await;
            }
        }
    }
}

fn diff_scope_sort_key(scope: ProjectDiffScope) -> u8 {
    match scope {
        ProjectDiffScope::Staged => 0,
        ProjectDiffScope::Unstaged => 1,
        ProjectDiffScope::Uncommitted => 2,
    }
}

async fn send_payload<T: serde::Serialize>(
    stream: &Stream,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("failed to serialize {kind} payload: {error}"))?;
    stream
        .send_value(kind, payload)
        .map_err(|_| "project stream closed".to_owned())
}

async fn emit_fatal_project_stream_error(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    operation: &str,
    message: String,
) {
    let streams = subscribers
        .values()
        .map(|subscriber| subscriber.stream.clone())
        .collect::<Vec<_>>();
    for stream in streams {
        emit_project_command_error(
            &stream,
            FrameKind::ProjectFileList,
            operation,
            message.clone(),
            true,
        )
        .await;
    }
    subscribers.clear();
}

fn project_watch_limit_guidance(error: &str) -> String {
    format!(
        "{error} Live project file updates are disabled, but the project remains available. For best results, configure each project root as the root of its Git repository instead of a broader parent directory. On Linux, you can also increase fs.inotify.max_user_watches. Reopen the project after correcting the root or system limit."
    )
}

async fn emit_project_stream_warning(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    operation: &str,
    message: String,
) {
    let streams = subscribers
        .values()
        .map(|subscriber| subscriber.stream.clone())
        .collect::<Vec<_>>();
    for stream in streams {
        emit_project_command_error(
            &stream,
            FrameKind::ProjectFileList,
            operation,
            message.clone(),
            false,
        )
        .await;
    }
}

async fn emit_project_command_error(
    stream: &Stream,
    request_kind: FrameKind,
    operation: &str,
    message: String,
    fatal: bool,
) {
    let payload = CommandErrorPayload {
        request_id: None,
        stream: stream.path().clone(),
        request_kind,
        operation: operation.to_owned(),
        code: CommandErrorCode::Internal,
        message,
        fatal,
    };
    match serde_json::to_value(payload) {
        Ok(payload) => {
            let _ = stream.send_value(FrameKind::CommandError, payload);
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to serialize project stream error payload"
            );
        }
    }
}

/// Default depth limit for initial and watched file listings.
/// Directories at this depth are listed but not recursed into.
const DEFAULT_FILE_LIST_DEPTH: usize = 2;

/// Scan the filesystem and return raw (path, kind) entries per root at the default depth.
pub(crate) fn scan_raw_entries(
    project: &Project,
) -> Result<BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>, String> {
    scan_raw_entries_with_depth(project, DEFAULT_FILE_LIST_DEPTH)
}

fn scan_raw_entries_with_depth(
    project: &Project,
    max_depth: usize,
) -> Result<BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>, String> {
    let mut result = BTreeMap::new();
    for root in project.root_paths() {
        let root_path = Path::new(&root.0);
        let metadata = fs::metadata(root_path)
            .map_err(|err| format!("Failed to stat project root '{}': {err}", root))?;
        if !metadata.is_dir() {
            return Err(format!("Project root '{}' is not a directory", root));
        }
        let mut raw = Vec::new();
        collect_raw_entries(root_path, root_path, &mut raw, 0, max_depth)?;
        result.insert(root, raw.into_iter().collect());
    }
    Ok(result)
}

/// List entries within a specific subdirectory of a root (all Add ops).
pub(crate) fn build_dir_listing(
    project: &Project,
    root: &ProjectRootPath,
    dir_relative_path: &str,
) -> Result<ProjectFileListPayload, String> {
    validate_root(project, root)?;
    if !dir_relative_path.is_empty() {
        validate_relative_path(dir_relative_path)?;
    }

    let root_path = Path::new(&root.0);
    let dir_path = if dir_relative_path.is_empty() {
        root_path.to_path_buf()
    } else {
        root_path.join(dir_relative_path)
    };

    let metadata = fs::metadata(&dir_path)
        .map_err(|err| format!("Failed to stat directory '{}': {err}", dir_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("'{}' is not a directory", dir_path.display()));
    }

    let mut raw = Vec::new();
    collect_raw_entries(root_path, &dir_path, &mut raw, 0, DEFAULT_FILE_LIST_DEPTH)?;

    let entries: Vec<ProjectFileEntry> = raw
        .into_iter()
        .map(|(path, kind)| ProjectFileEntry {
            relative_path: path,
            kind,
            op: FileEntryOp::Add,
        })
        .collect();

    Ok(ProjectFileListPayload {
        incremental: true,
        roots: vec![ProjectRootListing {
            root: root.clone(),
            entries,
        }],
    })
}

pub(crate) fn build_git_status(project: &Project) -> Result<ProjectGitStatusPayload, String> {
    build_git_status_with_runner(project, run_git_mode)
}

pub(crate) fn is_not_git_repository_error(error: &str) -> bool {
    error.to_ascii_lowercase().contains("not a git repository")
}

/// Whether git will place `root` inside a repository. Decided from the
/// filesystem rather than git's stderr: a damaged repository (unreadable
/// objects, broken HEAD) also reports "not a git repository", and that is a
/// failure to surface, not a clean tree.
pub(crate) fn root_is_git_repository(root: &str) -> bool {
    Path::new(root)
        .ancestors()
        .any(|dir| dir.join(".git").exists())
}

fn build_git_status_with_runner<F>(
    project: &Project,
    mut run_git: F,
) -> Result<ProjectGitStatusPayload, String>
where
    F: FnMut(&str, &[&str], GitAccessMode) -> Result<String, String>,
{
    let project_roots = project.root_paths();
    let mut roots = Vec::with_capacity(project_roots.len());

    for root in project_roots {
        let output = match run_git(
            &root.0,
            &["status", "--porcelain=v2", "--branch"],
            GitAccessMode::ReadOnly,
        ) {
            Ok(output) => output,
            Err(err) if is_not_git_repository_error(&err) => {
                roots.push(ProjectRootGitStatus {
                    root,
                    branch: None,
                    head_oid: None,
                    empty_tree_oid: None,
                    ahead: 0,
                    behind: 0,
                    clean: true,
                    files: Vec::new(),
                    recent_commits: Vec::new(),
                    history_has_more: false,
                });
                continue;
            }
            Err(err) => return Err(err),
        };
        let mut branch = None;
        let mut head_oid = None;
        let mut ahead = 0;
        let mut behind = 0;
        let mut files = BTreeMap::<String, ProjectGitFileStatus>::new();

        for line in output.lines() {
            if let Some(head) = line.strip_prefix("# branch.head ") {
                if head != "(detached)" {
                    branch = Some(head.to_owned());
                }
                continue;
            }

            if let Some(oid) = line.strip_prefix("# branch.oid ") {
                if oid != "(initial)" {
                    head_oid = Some(oid.to_owned());
                }
                continue;
            }

            if let Some(ab) = line.strip_prefix("# branch.ab ") {
                let parts: Vec<&str> = ab.split_whitespace().collect();
                assert_eq!(parts.len(), 2, "invalid branch.ab line: {}", line);
                ahead = parts[0]
                    .trim_start_matches('+')
                    .parse()
                    .unwrap_or_else(|err| panic!("invalid ahead count in '{}': {}", line, err));
                behind = parts[1]
                    .trim_start_matches('-')
                    .parse()
                    .unwrap_or_else(|err| panic!("invalid behind count in '{}': {}", line, err));
                continue;
            }

            if let Some(path) = line.strip_prefix("? ") {
                files.insert(
                    path.to_owned(),
                    ProjectGitFileStatus {
                        relative_path: path.to_owned(),
                        staged: None,
                        unstaged: None,
                        untracked: true,
                    },
                );
                continue;
            }

            if line.starts_with("u ") {
                let file = parse_unmerged_status_line(line)?;
                files.insert(file.relative_path.clone(), file);
                continue;
            }

            if line.starts_with("1 ") || line.starts_with("2 ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                assert!(parts.len() >= 9, "invalid porcelain status line '{}'", line);
                let xy = parts[1];
                assert_eq!(xy.len(), 2, "invalid XY status '{}'", xy);
                let path = line
                    .rsplit_once(' ')
                    .map(|(_, path)| path)
                    .unwrap_or_else(|| panic!("missing path in status line '{}'", line))
                    .split('\t')
                    .next()
                    .unwrap_or_else(|| panic!("missing path segment in status line '{}'", line))
                    .to_owned();
                files.insert(
                    path.clone(),
                    ProjectGitFileStatus {
                        relative_path: path,
                        staged: parse_change_kind(xy.as_bytes()[0] as char),
                        unstaged: parse_change_kind(xy.as_bytes()[1] as char),
                        untracked: false,
                    },
                );
            }
        }

        let history = load_empty_tree_oid(&root.0).and_then(|empty_tree_oid| {
            let (recent_commits, history_has_more) = if head_oid.is_some() {
                load_recent_commits(&root.0)?
            } else {
                (Vec::new(), false)
            };
            Ok((recent_commits, history_has_more, empty_tree_oid))
        });
        let (recent_commits, history_has_more, empty_tree_oid) = match history {
            Ok((recent_commits, history_has_more, empty_tree_oid)) => {
                (recent_commits, history_has_more, Some(empty_tree_oid))
            }
            Err(error) => {
                tracing::warn!(
                    root = %root.0,
                    %error,
                    "git history metadata is unavailable; continuing without recent history"
                );
                (Vec::new(), false, None)
            }
        };
        roots.push(ProjectRootGitStatus {
            root,
            branch,
            head_oid,
            empty_tree_oid,
            ahead,
            behind,
            clean: files.is_empty(),
            files: files.into_values().collect(),
            recent_commits,
            history_has_more,
        });
    }

    Ok(ProjectGitStatusPayload { roots })
}

fn load_recent_commits(root: &str) -> Result<(Vec<ProjectGitCommitSummary>, bool), String> {
    let fetch_count = (RECENT_HISTORY_LIMIT + 1).to_string();
    let raw = run_git_lossy_mode(
        root,
        &[
            "log",
            "--first-parent",
            "-n",
            &fetch_count,
            "--format=%H%x1f%P%x1f%s%x1f%an%x1f%at%x1e",
        ],
        GitAccessMode::ReadOnly,
    )?;
    let mut commits = Vec::new();
    for record in raw.split('\u{1e}') {
        let record = record.trim_matches(['\r', '\n']);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.split('\u{1f}');
        let oid = fields.next().unwrap_or_default();
        let parents = fields.next().unwrap_or_default();
        let subject = fields.next().unwrap_or_default();
        let author = fields.next().unwrap_or_default();
        let authored_at_seconds = fields
            .next()
            .unwrap_or_default()
            .parse::<i64>()
            .unwrap_or_default();
        validate_pinned_oid(oid)
            .map_err(|error| format!("git history returned an invalid commit identity: {error}"))?;
        if parents
            .split_whitespace()
            .any(|parent| validate_pinned_oid(parent).is_err())
        {
            return Err("git history returned an invalid parent commit identity".to_owned());
        }
        let parent_oids = parents.split_whitespace().collect::<Vec<_>>();
        commits.push(ProjectGitCommitSummary {
            oid: oid.to_owned(),
            first_parent_oid: parent_oids.first().map(|oid| (*oid).to_owned()),
            subject: if subject.is_empty() {
                "(no commit message)".to_owned()
            } else {
                subject.to_owned()
            },
            author: author.to_owned(),
            authored_at_seconds,
            is_merge: parent_oids.len() > 1,
        });
    }
    let history_has_more = commits.len() > RECENT_HISTORY_LIMIT;
    commits.truncate(RECENT_HISTORY_LIMIT);
    Ok((commits, history_has_more))
}

fn load_empty_tree_oid(root: &str) -> Result<String, String> {
    let oid = run_git_with_stdin_mode(
        root,
        &["hash-object", "-t", "tree", "--stdin"],
        "",
        GitAccessMode::ReadOnly,
    )?;
    let oid = oid.trim();
    validate_pinned_oid(oid)
        .map_err(|error| format!("git returned an invalid empty-tree identity: {error}"))?;
    Ok(oid.to_owned())
}

fn parse_unmerged_status_line(line: &str) -> Result<ProjectGitFileStatus, String> {
    let parts = line.splitn(11, ' ').collect::<Vec<_>>();
    let &[record, xy, sub, m1, m2, m3, mw, h1, h2, h3, path] = parts.as_slice() else {
        return Err(format!("invalid unmerged porcelain status line '{}'", line));
    };
    if record != "u"
        || [sub, m1, m2, m3, mw, h1, h2, h3]
            .into_iter()
            .any(|field| field.is_empty())
    {
        return Err(format!("invalid unmerged porcelain status line '{}'", line));
    }
    if xy.len() != 2 {
        return Err(format!("invalid unmerged XY status '{}' in '{}'", xy, line));
    }
    if path.is_empty() {
        return Err(format!("missing path in unmerged status line '{}'", line));
    }
    Ok(ProjectGitFileStatus {
        relative_path: path.to_owned(),
        staged: Some(ProjectGitChangeKind::Unmerged),
        unstaged: Some(ProjectGitChangeKind::Unmerged),
        untracked: false,
    })
}

pub(crate) fn read_file(
    project: &Project,
    payload: ProjectReadFilePayload,
) -> Result<ProjectFileContentsPayload, String> {
    let path = normalize_read_path(project, payload.path)?;
    validate_project_path(project, &path)?;
    let absolute = absolute_project_path(&path)?;
    // `version` is a placeholder here; the project-stream actor overwrites it
    // with the centralized counter's next value (the single bump point) before
    // the payload leaves the actor. See `ProjectStreamCommand::ReadFile`.
    let metadata = match fs::metadata(&absolute) {
        Ok(metadata) => metadata,
        // Deletion is an answer, not a failure: report it as a typed payload
        // (a command error carries no path, so the client could not attribute
        // it to the right viewer).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectFileContentsPayload {
                path,
                version: ProjectFileVersion(0),
                contents: None,
                is_binary: false,
                missing: true,
                binary: None,
            });
        }
        Err(err) => {
            return Ok(unreadable_file_payload(path, &absolute, err));
        }
    };
    let size_bytes = metadata.len();
    let mut prefix = Vec::new();
    if let Err(err) = File::open(&absolute).and_then(|file| {
        file.take(CONTENT_SNIFF_BYTES)
            .read_to_end(&mut prefix)
            .map(|_| ())
    }) {
        return Ok(unreadable_file_payload(path, &absolute, err));
    }
    let sniffed_mime = sniff_binary_mime(&prefix);
    let prefix_is_binary =
        sniffed_mime.is_some() || prefix.contains(&0) || std::str::from_utf8(&prefix).is_err();
    if prefix_is_binary && size_bytes > BINARY_PREVIEW_LIMIT_BYTES {
        return Ok(ProjectFileContentsPayload {
            path,
            version: ProjectFileVersion(0),
            contents: None,
            is_binary: true,
            missing: false,
            binary: Some(ProjectBinaryFilePayload {
                mime_type: sniffed_mime
                    .or_else(|| binary_mime_from_path(&absolute))
                    .unwrap_or("application/octet-stream")
                    .to_owned(),
                size_bytes,
                data_base64: None,
                preview_error: Some(format!(
                    "Preview unavailable because this file exceeds the {} MiB limit.",
                    BINARY_PREVIEW_LIMIT_BYTES / 1024 / 1024
                )),
            }),
        });
    }
    let bytes = match fs::read(&absolute) {
        Ok(bytes) => bytes,
        Err(err) => return Ok(unreadable_file_payload(path, &absolute, err)),
    };
    if prefix_is_binary {
        let mime_type = sniffed_mime
            .or_else(|| binary_mime_from_path(&absolute))
            .unwrap_or("application/octet-stream");
        return Ok(binary_file_payload(path, bytes, mime_type, size_bytes));
    }
    match String::from_utf8(bytes) {
        Ok(contents) => Ok(ProjectFileContentsPayload {
            path,
            version: ProjectFileVersion(0),
            contents: Some(contents),
            is_binary: false,
            missing: false,
            binary: None,
        }),
        Err(error) => Ok(binary_file_payload(
            path,
            error.into_bytes(),
            binary_mime_from_path(&absolute).unwrap_or("application/octet-stream"),
            size_bytes,
        )),
    }
}

fn unreadable_file_payload(
    path: ProjectPath,
    absolute: &Path,
    error: std::io::Error,
) -> ProjectFileContentsPayload {
    ProjectFileContentsPayload {
        path,
        version: ProjectFileVersion(0),
        contents: Some(format!(
            "Unable to read this file.\n\n{}: {error}",
            absolute.display()
        )),
        is_binary: false,
        missing: false,
        binary: None,
    }
}

fn binary_file_payload(
    path: ProjectPath,
    bytes: Vec<u8>,
    mime_type: &str,
    size_bytes: u64,
) -> ProjectFileContentsPayload {
    let supported = is_supported_preview_mime(mime_type);
    ProjectFileContentsPayload {
        path,
        version: ProjectFileVersion(0),
        contents: None,
        is_binary: true,
        missing: false,
        binary: Some(ProjectBinaryFilePayload {
            mime_type: mime_type.to_owned(),
            size_bytes,
            data_base64: supported.then(|| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(bytes)
            }),
            preview_error: (!supported)
                .then(|| "Tyde does not have an inline preview for this file type.".to_owned()),
        }),
    }
}

fn sniff_binary_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("video/mp4")
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some("video/webm")
    } else if bytes.starts_with(b"ID3") || looks_like_mp3_frame(bytes) {
        Some("audio/mpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if bytes.starts_with(b"fLaC") {
        Some("audio/flac")
    } else {
        None
    }
}

fn looks_like_mp3_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0
}

fn binary_mime_from_path(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "mp4" | "m4v" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" | "oga" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "pdf" => Some("application/pdf"),
        _ => None,
    }
}

fn is_supported_preview_mime(mime_type: &str) -> bool {
    mime_type.starts_with("image/")
        || mime_type.starts_with("video/")
        || mime_type.starts_with("audio/")
        || mime_type == "application/pdf"
}

// ── Project global search ─────────────────────────────────────────────────

/// Maximum number of matching files returned by a single search.
const MAX_SEARCH_FILES: usize = 1000;
/// Maximum total matches across all files before the walk is truncated.
const MAX_SEARCH_MATCHES: usize = 10_000;
/// Maximum matches reported for any single file.
const MAX_MATCHES_PER_FILE: usize = 1000;
/// Matching line text longer than this (in bytes) is truncated before being
/// sent. Match ranges are computed against the truncated text so they always
/// stay in bounds.
const MAX_SEARCH_LINE_BYTES: usize = 2000;

/// Final totals for a completed (or aborted) search walk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SearchSummary {
    pub total_files: u32,
    pub total_matches: u32,
    pub truncated: bool,
    pub cancelled: bool,
}

/// Pure search core. Walks `payload.roots` (or every project root when empty),
/// honouring `.gitignore` unless `include_ignored` is set, and invokes `emit`
/// once per matching file. `emit` returns `false` to stop early (e.g. the
/// output stream closed). `cancelled` is polled between files so a superseding
/// search or an explicit cancel aborts the walk promptly.
pub(crate) fn search_project<E, C>(
    project: &Project,
    payload: &ProjectSearchPayload,
    mut emit: E,
    cancelled: C,
) -> Result<SearchSummary, String>
where
    E: FnMut(ProjectSearchFileResult) -> bool,
    C: Fn() -> bool,
{
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::{BinaryDetection, SearcherBuilder};
    use ignore::WalkBuilder;

    if payload.query.is_empty() {
        return Err("search query must not be empty".to_owned());
    }

    let pattern = if payload.use_regex {
        payload.query.clone()
    } else {
        regex::escape(&payload.query)
    };
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!payload.case_sensitive)
        .word(payload.whole_word)
        .build(&pattern)
        .map_err(|err| format!("invalid search pattern: {err}"))?;

    let all_roots = project.root_paths();
    let roots: Vec<ProjectRootPath> = if payload.roots.is_empty() {
        all_roots
    } else {
        for requested in &payload.roots {
            if !all_roots.iter().any(|candidate| candidate == requested) {
                return Err(format!(
                    "Root '{}' does not belong to project {}",
                    requested, project.id
                ));
            }
        }
        payload.roots.clone()
    };

    let max_files = payload
        .max_results
        .map(|value| value as usize)
        .unwrap_or(MAX_SEARCH_FILES)
        .min(MAX_SEARCH_FILES);

    let path_prefix = payload
        .path_prefix
        .as_deref()
        .map(|prefix| {
            prefix
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_owned()
        })
        .filter(|prefix| !prefix.is_empty());

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut summary = SearchSummary::default();

    'outer: for root in &roots {
        let root_path = Path::new(&root.0);
        let respect_ignore = !payload.include_ignored;
        let mut builder = WalkBuilder::new(root_path);
        builder
            .hidden(respect_ignore)
            .ignore(respect_ignore)
            .git_ignore(respect_ignore)
            .git_global(respect_ignore)
            .git_exclude(respect_ignore)
            // Honour .gitignore even when the root is not inside a git repo,
            // matching editor "search" semantics for standalone folders.
            .require_git(false)
            .parents(respect_ignore)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git");
        let walk = builder.build();

        for result in walk {
            if cancelled() {
                summary.cancelled = true;
                break 'outer;
            }
            if summary.total_files as usize >= max_files {
                summary.truncated = true;
                break 'outer;
            }
            let remaining_matches =
                MAX_SEARCH_MATCHES.saturating_sub(summary.total_matches as usize);
            if remaining_matches == 0 {
                summary.truncated = true;
                break 'outer;
            }

            let entry = match result {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let is_file = entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false);
            if !is_file {
                continue;
            }
            let path = entry.path();
            let relative_path = match path.strip_prefix(root_path) {
                Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if let Some(prefix) = &path_prefix {
                let matches_prefix =
                    relative_path == *prefix || relative_path.starts_with(&format!("{prefix}/"));
                if !matches_prefix {
                    continue;
                }
            }

            // Cap this file's matches at the smaller of the per-file limit and
            // the remaining global budget, so the total can never overshoot.
            let per_file_cap = MAX_MATCHES_PER_FILE.min(remaining_matches);
            let mut collector = SearchMatchCollector::new(&matcher, per_file_cap);
            if searcher
                .search_path(&matcher, path, &mut collector)
                .is_err()
            {
                continue;
            }
            if collector.matches.is_empty() {
                continue;
            }

            summary.total_files += 1;
            summary.total_matches += collector.matches.len() as u32;
            let file_result = ProjectSearchFileResult {
                path: ProjectPath {
                    root: root.clone(),
                    relative_path,
                },
                matches: collector.matches,
                truncated: collector.truncated,
            };
            if !emit(file_result) {
                summary.cancelled = true;
                break 'outer;
            }
            // If this file exhausted the global match budget, the run is
            // truncated — record it now since the loop may end here.
            if summary.total_matches as usize >= MAX_SEARCH_MATCHES {
                summary.truncated = true;
                break 'outer;
            }
        }
    }

    Ok(summary)
}

/// `grep` sink that records each matching line along with the byte ranges of
/// the matches *within the exact text we send to the client*.
struct SearchMatchCollector<'matcher> {
    matcher: &'matcher grep_regex::RegexMatcher,
    matches: Vec<ProjectSearchMatch>,
    /// Maximum matches this collector will record before stopping. The caller
    /// sets it to `min(per-file cap, remaining global budget)` so a single file
    /// can never push the run past the global match cap.
    max_matches: usize,
    truncated: bool,
}

impl<'matcher> SearchMatchCollector<'matcher> {
    fn new(matcher: &'matcher grep_regex::RegexMatcher, max_matches: usize) -> Self {
        Self {
            matcher,
            matches: Vec::new(),
            max_matches,
            truncated: false,
        }
    }
}

impl grep_searcher::Sink for SearchMatchCollector<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        use grep_matcher::Matcher;

        if self.matches.len() >= self.max_matches {
            self.truncated = true;
            return Ok(false);
        }

        let line_number = mat.line_number().unwrap_or(0) as u32;
        let mut raw = mat.bytes();
        if let Some(stripped) = raw.strip_suffix(b"\n") {
            raw = stripped;
        }
        if let Some(stripped) = raw.strip_suffix(b"\r") {
            raw = stripped;
        }
        // Send the line as lossy UTF-8, then compute match ranges against the
        // bytes we actually send so client-side slicing is always consistent.
        let line_text = truncate_to_bytes(&String::from_utf8_lossy(raw), MAX_SEARCH_LINE_BYTES);
        let mut ranges = Vec::new();
        self.matcher
            .find_iter(line_text.as_bytes(), |found| {
                ranges.push((found.start() as u32, found.end() as u32));
                true
            })
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        self.matches.push(ProjectSearchMatch {
            line_number,
            line_text,
            ranges,
        });
        Ok(true)
    }
}

/// Truncate a string to at most `max_bytes` bytes without splitting a `char`.
fn truncate_to_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

pub(crate) fn read_diff(
    project: &Project,
    payload: ProjectReadDiffPayload,
) -> Result<ProjectGitDiffPayload, String> {
    read_diff_with_runner(project, payload, run_git_mode)
}

fn read_diff_with_runner<F>(
    project: &Project,
    payload: ProjectReadDiffPayload,
    mut run_git: F,
) -> Result<ProjectGitDiffPayload, String>
where
    F: FnMut(&str, &[&str], GitAccessMode) -> Result<String, String>,
{
    validate_root(project, &payload.root)?;
    if let Some(path) = &payload.path {
        validate_relative_path(path)?;
    }

    let mut args = vec![
        "diff",
        "--relative",
        git_diff_context_arg(payload.context_mode),
    ];
    match &payload.revision {
        ProjectDiffRevision::WorkingTree => match payload.scope {
            ProjectDiffScope::Staged => args.push("--cached"),
            ProjectDiffScope::Unstaged => {}
            ProjectDiffScope::Uncommitted => args.push("HEAD"),
        },
        ProjectDiffRevision::CommittedRange { base_oid, tip_oid } => {
            validate_pinned_oid(base_oid)?;
            validate_pinned_oid(tip_oid)?;
            args.push(base_oid);
            args.push(tip_oid);
        }
    }
    if let Some(path) = &payload.path {
        args.push("--");
        args.push(path);
    }

    let raw = run_git(&payload.root.0, &args, GitAccessMode::ReadOnly)?;
    let mut files = parse_git_diff(&raw)?;
    if matches!(payload.revision, ProjectDiffRevision::WorkingTree)
        && matches!(
            payload.scope,
            ProjectDiffScope::Unstaged | ProjectDiffScope::Uncommitted
        )
    {
        let untracked_paths = list_untracked_paths_with_runner(
            &payload.root.0,
            payload.path.as_deref(),
            &mut run_git,
        )?;
        for relative_path in untracked_paths {
            // `ls-files --others` reports a nested repository as `dir/`; it
            // has no content this repository can stage or review.
            if relative_path.ends_with('/') {
                continue;
            }
            files.push(build_untracked_diff_file(&payload.root.0, &relative_path)?);
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    }

    Ok(ProjectGitDiffPayload {
        request_id: payload.request_id,
        root: payload.root,
        scope: payload.scope,
        revision: payload.revision,
        path: payload.path,
        context_mode: payload.context_mode,
        files,
    })
}

fn validate_pinned_oid(oid: &str) -> Result<(), String> {
    if matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("committed diff endpoints must be full hexadecimal object IDs".to_owned())
    }
}

pub(crate) fn committed_range_commit_count(
    project: &Project,
    root: &ProjectRootPath,
    base_oid: &str,
    tip_oid: &str,
) -> Result<u32, String> {
    validate_root(project, root)?;
    validate_pinned_oid(base_oid)?;
    validate_pinned_oid(tip_oid)?;
    if base_oid == tip_oid {
        return Err("committed review range must contain at least one commit".to_owned());
    }

    let max_count = format!("--max-count={RECENT_HISTORY_LIMIT}");
    let raw = run_git_mode(
        &root.0,
        &[
            "rev-list",
            "--first-parent",
            "--parents",
            &max_count,
            tip_oid,
        ],
        GitAccessMode::ReadOnly,
    )?;
    let mut expected_oid = tip_oid;
    let mut commit_count = 0_usize;
    let mut boundary_beyond_limit = false;
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let oid = fields
            .next()
            .ok_or_else(|| "git returned an empty first-parent history row".to_owned())?;
        validate_pinned_oid(oid).map_err(|error| {
            format!("git returned an invalid first-parent commit identity: {error}")
        })?;
        if oid != expected_oid {
            return Err(format!(
                "git first-parent history skipped expected commit {expected_oid}"
            ));
        }
        commit_count += 1;

        let first_parent = fields.next();
        if first_parent == Some(base_oid) {
            return u32::try_from(commit_count)
                .map_err(|_| "committed review range is too large".to_owned());
        }
        match first_parent {
            Some(parent_oid) => {
                validate_pinned_oid(parent_oid).map_err(|error| {
                    format!("git returned an invalid first-parent identity: {error}")
                })?;
                expected_oid = parent_oid;
                boundary_beyond_limit = commit_count == RECENT_HISTORY_LIMIT;
            }
            None => {
                let empty_tree_oid = load_empty_tree_oid(&root.0)?;
                if base_oid == empty_tree_oid {
                    return u32::try_from(commit_count)
                        .map_err(|_| "committed review range is too large".to_owned());
                }
                break;
            }
        }
    }

    if boundary_beyond_limit {
        return Err(format!(
            "committed review base {base_oid} was not reached within the \
             {RECENT_HISTORY_LIMIT}-commit recent-history limit from tip {tip_oid}"
        ));
    }

    Err(format!(
        "committed review base {base_oid} is not the first-parent boundary of tip {tip_oid}"
    ))
}

fn git_diff_context_arg(context_mode: DiffContextMode) -> &'static str {
    match context_mode {
        DiffContextMode::Hunks => "-U3",
        DiffContextMode::FullFile => "-U9999999",
    }
}

fn list_untracked_paths_with_runner<F>(
    root: &str,
    path: Option<&str>,
    run_git: &mut F,
) -> Result<Vec<String>, String>
where
    F: FnMut(&str, &[&str], GitAccessMode) -> Result<String, String>,
{
    if let Some(path) = path {
        validate_relative_path(path)?;
    }

    let mut args = vec!["ls-files", "--others", "--exclude-standard"];
    if let Some(path) = path {
        args.push("--");
        args.push(path);
    }

    let raw = run_git(root, &args, GitAccessMode::ReadOnly)?;
    Ok(raw
        .lines()
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn build_untracked_diff_file(
    root: &str,
    relative_path: &str,
) -> Result<ProjectGitDiffFile, String> {
    validate_relative_path(relative_path)?;
    let absolute_path = Path::new(root).join(relative_path);
    let bytes = match fs::read(&absolute_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Err(format!(
                "Failed to read untracked file '{}' from '{}': {error}",
                relative_path, root
            ));
        }
    };
    let contents = match String::from_utf8(bytes) {
        Ok(contents) if !contents.contains('\0') => contents,
        Ok(_) | Err(_) => {
            return Ok(ProjectGitDiffFile {
                relative_path: relative_path.to_owned(),
                change_kind: Some(ProjectGitChangeKind::Added),
                is_binary: true,
                unmerged: false,
                hunks: Vec::new(),
            });
        }
    };

    let hunks = if contents.is_empty() {
        Vec::new()
    } else {
        let lines = contents
            .lines()
            .enumerate()
            .map(|(index, line)| ProjectGitDiffLine {
                kind: ProjectGitDiffLineKind::Added,
                text: line.to_owned(),
                old_line_number: None,
                new_line_number: Some((index + 1) as u32),
            })
            .collect::<Vec<_>>();
        vec![ProjectGitDiffHunk {
            hunk_id: build_hunk_id(relative_path, 0),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: lines.len() as u32,
            lines,
        }]
    };

    Ok(ProjectGitDiffFile {
        relative_path: relative_path.to_owned(),
        change_kind: Some(ProjectGitChangeKind::Added),
        is_binary: false,
        unmerged: false,
        hunks,
    })
}

pub(crate) fn stage_file(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_project_path(project, path)?;
    run_git_mode(
        &path.root.0,
        &["add", "--", &path.relative_path],
        GitAccessMode::Mutating,
    )?;
    Ok(())
}

pub(crate) fn stage_hunk(
    project: &Project,
    path: &ProjectPath,
    hunk_id: &str,
) -> Result<(), String> {
    validate_project_path(project, path)?;
    if hunk_id.trim().is_empty() {
        return Err("project_stage_hunk hunk_id must not be empty".to_owned());
    }

    let raw = run_git_mode(
        &path.root.0,
        &["diff", "--", &path.relative_path],
        GitAccessMode::ReadOnly,
    )?;
    let parsed = parse_raw_git_diff(&raw)?;
    let Some(file) = parsed
        .iter()
        .find(|file| file.relative_path == path.relative_path)
    else {
        return Err(format!(
            "No unstaged diff exists for '{}'",
            path.relative_path
        ));
    };

    let Some((_, hunk)) = file
        .hunks
        .iter()
        .enumerate()
        .find(|(index, _)| build_hunk_id(&file.relative_path, *index) == hunk_id)
    else {
        return Err(format!(
            "Unknown hunk id '{}' for '{}'",
            hunk_id, path.relative_path
        ));
    };

    let mut patch = String::new();
    for line in &file.header_lines {
        patch.push_str(line);
        patch.push('\n');
    }
    patch.push_str(&hunk.header);
    patch.push('\n');
    for line in &hunk.lines {
        patch.push_str(line);
        patch.push('\n');
    }

    run_git_with_stdin_mode(
        &path.root.0,
        &["apply", "--cached", "--recount", "--whitespace=nowarn", "-"],
        &patch,
        GitAccessMode::Mutating,
    )?;
    Ok(())
}

pub(crate) fn unstage_file(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_project_path(project, path)?;
    let result = run_git_mode(
        &path.root.0,
        &["restore", "--staged", "--", &path.relative_path],
        GitAccessMode::Mutating,
    );
    match result {
        Ok(_) => Ok(()),
        Err(err) if err.contains("bad default revision") || err.contains("unknown revision") => {
            // Empty repo with no HEAD: restore --staged fails. Use rm --cached.
            run_git_mode(
                &path.root.0,
                &["rm", "--cached", "--", &path.relative_path],
                GitAccessMode::Mutating,
            )?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn discard_file(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_project_path(project, path)?;
    // checkout restores tracked files; clean removes untracked files.
    // One will fail harmlessly depending on file state.
    let checkout_ok = run_git_mode(
        &path.root.0,
        &["checkout", "--", &path.relative_path],
        GitAccessMode::Mutating,
    )
    .is_ok();
    let clean_ok = run_git_mode(
        &path.root.0,
        &["clean", "-f", "--", &path.relative_path],
        GitAccessMode::Mutating,
    )
    .is_ok();
    if checkout_ok || clean_ok {
        Ok(())
    } else {
        Err(format!(
            "Failed to discard changes for '{}'",
            path.relative_path
        ))
    }
}

pub(crate) fn commit(
    project: &Project,
    root: &ProjectRootPath,
    message: &str,
) -> Result<String, String> {
    validate_root(project, root)?;
    run_git_mode(&root.0, &["commit", "-m", message], GitAccessMode::Mutating)?;
    let hash = run_git_mode(&root.0, &["rev-parse", "HEAD"], GitAccessMode::ReadOnly)?;
    Ok(hash.trim().to_owned())
}

fn collect_raw_entries(
    root: &Path,
    current: &Path,
    out: &mut Vec<RawFileEntry>,
    depth: usize,
    max_depth: usize,
) -> Result<(), String> {
    let mut entries = fs::read_dir(current)
        .map_err(|err| format!("Failed to read directory '{}': {err}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("Failed to iterate directory '{}': {err}", current.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }

        let metadata = fs::symlink_metadata(&path)
            .map_err(|err| format!("Failed to stat path '{}': {err}", path.display()))?;
        let relative_path = path
            .strip_prefix(root)
            .map_err(|err| {
                format!(
                    "failed to strip root prefix from '{}': {}",
                    path.display(),
                    err
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");

        let kind = if metadata.file_type().is_symlink() {
            ProjectFileKind::Symlink
        } else if metadata.is_dir() {
            ProjectFileKind::Directory
        } else {
            ProjectFileKind::File
        };

        out.push((relative_path, kind));

        // Recurse into directories only if within depth limit
        if metadata.is_dir() && depth < max_depth {
            collect_raw_entries(root, &path, out, depth + 1, max_depth)?;
        }
    }

    Ok(())
}

fn validate_root(project: &Project, root: &ProjectRootPath) -> Result<(), String> {
    if project
        .root_paths()
        .into_iter()
        .any(|candidate| candidate == *root)
    {
        return Ok(());
    }
    Err(format!(
        "Root '{}' does not belong to project {}",
        root, project.id
    ))
}

fn validate_project_path(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_root(project, &path.root)?;
    validate_relative_path(&path.relative_path)
}

fn normalize_read_path(project: &Project, path: ProjectPath) -> Result<ProjectPath, String> {
    let normalized_relative_path = normalize_file_reference(&path.relative_path)?;

    if let Some(path) = project_path_from_absolute(project, &normalized_relative_path) {
        return Ok(path);
    }

    Ok(ProjectPath {
        root: path.root,
        relative_path: normalized_relative_path,
    })
}

fn normalize_file_reference(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("relative path must not be empty".to_owned());
    }

    let decoded = percent_decode_path(trimmed).unwrap_or_else(|| trimmed.to_owned());
    let without_scheme = decoded.strip_prefix("file://").unwrap_or(decoded.as_str());
    let without_fragment = without_scheme.split('#').next().unwrap_or(without_scheme);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let without_line_suffix = strip_trailing_line_suffix(without_query);
    let normalized = without_line_suffix.trim_start_matches("./");

    if normalized.trim().is_empty() {
        return Err("relative path must not be empty".to_owned());
    }

    Ok(normalized.to_owned())
}

fn strip_trailing_line_suffix(path: &str) -> &str {
    let mut candidate = path;
    for _ in 0..2 {
        let Some((prefix, suffix)) = candidate.rsplit_once(':') else {
            break;
        };
        if suffix.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = prefix;
        } else {
            break;
        }
    }
    candidate
}

fn project_path_from_absolute(project: &Project, absolute_path: &str) -> Option<ProjectPath> {
    let absolute = Path::new(absolute_path);
    if !absolute.is_absolute() {
        return None;
    }

    for root in project.root_paths() {
        let Ok(relative) = absolute.strip_prefix(&root.0) else {
            continue;
        };
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        if relative_path.is_empty() {
            return None;
        }
        return Some(ProjectPath {
            root,
            relative_path,
        });
    }

    None
}

fn percent_decode_path(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        match byte {
            b'%' => {
                let high = chars.next()?;
                let low = chars.next()?;
                let decoded = (decode_hex_nibble(high)? << 4) | decode_hex_nibble(low)?;
                bytes.push(decoded);
            }
            _ => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("relative path must not be empty".to_owned());
    }

    let relative = Path::new(path);
    if !relative.is_relative() {
        return Err(format!("project relative path must be relative: {}", path));
    }

    for component in relative.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "project relative path must not contain '..': {}",
                    path
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("project relative path must be relative: {}", path));
            }
        }
    }

    Ok(())
}

fn absolute_project_path(path: &ProjectPath) -> Result<PathBuf, String> {
    validate_relative_path(&path.relative_path)?;
    Ok(Path::new(&path.root.0).join(&path.relative_path))
}

fn run_git_mode(root: &str, args: &[&str], access_mode: GitAccessMode) -> Result<String, String> {
    run_git_mode_with_binary("git", root, args, access_mode)
}

fn run_git_lossy_mode(
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Result<String, String> {
    let output = git_command("git", root, args, access_mode)
        .output()
        .map_err(|err| format!("Failed to run git in '{}': {err}", root))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed in '{}': {}",
            args,
            root,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_git_mode_with_binary(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Result<String, String> {
    let output = git_command(git_binary, root, args, access_mode)
        .output()
        .map_err(|err| format!("Failed to run git in '{}': {err}", root))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed in '{}': {}",
            args,
            root,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("git output was not valid UTF-8 in '{}': {err}", root))
}

fn run_git_with_stdin_mode(
    root: &str,
    args: &[&str],
    stdin: &str,
    access_mode: GitAccessMode,
) -> Result<String, String> {
    run_git_with_stdin_mode_with_binary("git", root, args, stdin, access_mode)
}

fn run_git_with_stdin_mode_with_binary(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    stdin: &str,
    access_mode: GitAccessMode,
) -> Result<String, String> {
    let mut child = git_command(git_binary, root, args, access_mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Failed to start git in '{}': {err}", root))?;

    use std::io::Write;
    let mut stdin_pipe = child
        .stdin
        .take()
        .unwrap_or_else(|| panic!("git stdin pipe missing for args {:?}", args));
    stdin_pipe
        .write_all(stdin.as_bytes())
        .map_err(|err| format!("Failed to write git stdin in '{}': {err}", root))?;
    drop(stdin_pipe);

    let output = child
        .wait_with_output()
        .map_err(|err| format!("Failed to wait for git in '{}': {err}", root))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed in '{}': {}",
            args,
            root,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|err| format!("git output was not valid UTF-8 in '{}': {err}", root))
}

fn git_command(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Command {
    let mut command = Command::new(git_binary);
    command.env("LC_ALL", "C");
    if matches!(access_mode, GitAccessMode::ReadOnly) {
        command.arg("--no-optional-locks");
    }
    command.arg("-C").arg(root).args(args);
    command
}

fn parse_change_kind(status: char) -> Option<ProjectGitChangeKind> {
    match status {
        '.' | ' ' => None,
        'A' => Some(ProjectGitChangeKind::Added),
        'M' => Some(ProjectGitChangeKind::Modified),
        'D' => Some(ProjectGitChangeKind::Deleted),
        'R' => Some(ProjectGitChangeKind::Renamed),
        'C' => Some(ProjectGitChangeKind::Copied),
        'T' => Some(ProjectGitChangeKind::TypeChanged),
        other => panic!("unsupported git change kind '{}'", other),
    }
}

#[derive(Debug, Clone)]
struct ParsedGitDiffFile {
    relative_path: String,
    header_lines: Vec<String>,
    unmerged: bool,
    hunks: Vec<ParsedGitDiffHunk>,
}

#[derive(Debug, Clone)]
struct ParsedGitDiffHunk {
    header: String,
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    lines: Vec<String>,
}

fn parse_git_diff(raw: &str) -> Result<Vec<ProjectGitDiffFile>, String> {
    parse_raw_git_diff(raw)?
        .into_iter()
        .map(|file| {
            let relative_path = file.relative_path.clone();
            let change_kind = parsed_git_diff_file_change_kind(&file);
            Ok(ProjectGitDiffFile {
                relative_path: relative_path.clone(),
                change_kind: Some(change_kind),
                is_binary: parsed_git_diff_file_is_binary(&file),
                unmerged: file.unmerged,
                hunks: file
                    .hunks
                    .into_iter()
                    .enumerate()
                    .map(|(index, hunk)| {
                        let lines = project_git_diff_lines_for_hunk(&hunk);
                        ProjectGitDiffHunk {
                            hunk_id: build_hunk_id(&relative_path, index),
                            old_start: hunk.old_start,
                            old_count: hunk.old_count,
                            new_start: hunk.new_start,
                            new_count: hunk.new_count,
                            lines,
                        }
                    })
                    .collect(),
            })
        })
        .collect()
}

fn parsed_git_diff_file_change_kind(file: &ParsedGitDiffFile) -> ProjectGitChangeKind {
    if file.unmerged {
        return ProjectGitChangeKind::Unmerged;
    }
    if file
        .header_lines
        .iter()
        .any(|line| line.starts_with("new file mode "))
    {
        return ProjectGitChangeKind::Added;
    }
    if file
        .header_lines
        .iter()
        .any(|line| line.starts_with("deleted file mode "))
    {
        return ProjectGitChangeKind::Deleted;
    }
    if file
        .header_lines
        .iter()
        .any(|line| line.starts_with("rename from "))
    {
        return ProjectGitChangeKind::Renamed;
    }
    if file
        .header_lines
        .iter()
        .any(|line| line.starts_with("copy from "))
    {
        return ProjectGitChangeKind::Copied;
    }
    ProjectGitChangeKind::Modified
}

fn parsed_git_diff_file_is_binary(file: &ParsedGitDiffFile) -> bool {
    file.header_lines
        .iter()
        .any(|line| is_git_binary_diff_marker(line))
}

fn is_git_binary_diff_marker(line: &str) -> bool {
    line.starts_with("Binary files ") || line == "GIT binary patch"
}

fn parse_raw_git_diff(raw: &str) -> Result<Vec<ParsedGitDiffFile>, String> {
    let mut files = Vec::new();
    let mut current_file: Option<ParsedGitDiffFile> = None;
    let mut current_hunk: Option<ParsedGitDiffHunk> = None;

    for line in raw.lines() {
        if let Some(relative_path) = line
            .strip_prefix("diff --cc ")
            .or_else(|| line.strip_prefix("diff --combined "))
        {
            finish_parsed_git_diff_file(
                &mut files,
                &mut current_file,
                &mut current_hunk,
                "hunk appeared before file in git diff",
            )?;
            if relative_path.is_empty() {
                return Err(format!("invalid combined diff header '{}'", line));
            }
            current_file = Some(ParsedGitDiffFile {
                relative_path: relative_path.to_owned(),
                header_lines: vec![line.to_owned()],
                unmerged: true,
                hunks: Vec::new(),
            });
            continue;
        }

        if let Some(diff_line) = line.strip_prefix("diff --git ") {
            finish_parsed_git_diff_file(
                &mut files,
                &mut current_file,
                &mut current_hunk,
                "hunk appeared before file in git diff",
            )?;

            let parts: Vec<&str> = diff_line.split_whitespace().collect();
            if parts.len() != 2 {
                return Err(format!("invalid diff header '{}'", line));
            }
            current_file = Some(ParsedGitDiffFile {
                relative_path: parse_diff_path(parts[0], parts[1]),
                header_lines: vec![line.to_owned()],
                unmerged: false,
                hunks: Vec::new(),
            });
            continue;
        }

        if current_file.as_ref().is_some_and(|file| file.unmerged) {
            current_file
                .as_mut()
                .expect("unmerged file checked above")
                .header_lines
                .push(line.to_owned());
            continue;
        }

        if line.starts_with("@@") {
            if let Some(hunk) = current_hunk.take() {
                let file = current_file
                    .as_mut()
                    .ok_or_else(|| "hunk appeared before file in git diff".to_owned())?;
                file.hunks.push(hunk);
            }
            let (old_start, old_count, new_start, new_count) = parse_hunk_header(line)?;
            current_hunk = Some(ParsedGitDiffHunk {
                header: line.to_owned(),
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            hunk.lines.push(line.to_owned());
            continue;
        }

        if let Some(file) = current_file.as_mut() {
            file.header_lines.push(line.to_owned());
        }
    }

    finish_parsed_git_diff_file(
        &mut files,
        &mut current_file,
        &mut current_hunk,
        "trailing hunk appeared before file in git diff",
    )?;

    Ok(files)
}

fn finish_parsed_git_diff_file(
    files: &mut Vec<ParsedGitDiffFile>,
    current_file: &mut Option<ParsedGitDiffFile>,
    current_hunk: &mut Option<ParsedGitDiffHunk>,
    hunk_before_file_error: &'static str,
) -> Result<(), String> {
    if let Some(hunk) = current_hunk.take() {
        let file = current_file
            .as_mut()
            .ok_or_else(|| hunk_before_file_error.to_owned())?;
        file.hunks.push(hunk);
    }
    if let Some(file) = current_file.take() {
        files.push(file);
    }
    Ok(())
}

fn parse_diff_path(a_path: &str, b_path: &str) -> String {
    if let Some(path) = b_path.strip_prefix("b/")
        && path != "dev/null"
    {
        return path.to_owned();
    }
    a_path.strip_prefix("a/").unwrap_or(a_path).to_owned()
}

fn build_hunk_id(relative_path: &str, index: usize) -> String {
    format!("{}::{}", relative_path, index)
}

fn project_git_diff_lines_for_hunk(hunk: &ParsedGitDiffHunk) -> Vec<ProjectGitDiffLine> {
    let mut old_line = hunk.old_start;
    let mut new_line = hunk.new_start;

    hunk.lines
        .iter()
        .map(|line| {
            let kind = classify_diff_line(line);
            if is_git_no_newline_marker(line) {
                return ProjectGitDiffLine {
                    kind,
                    text: line.to_owned(),
                    old_line_number: None,
                    new_line_number: None,
                };
            }

            match kind {
                ProjectGitDiffLineKind::Context => {
                    let parsed = ProjectGitDiffLine {
                        kind,
                        text: strip_diff_line_prefix(line).to_owned(),
                        old_line_number: Some(old_line),
                        new_line_number: Some(new_line),
                    };
                    old_line += 1;
                    new_line += 1;
                    parsed
                }
                ProjectGitDiffLineKind::Added => {
                    let parsed = ProjectGitDiffLine {
                        kind,
                        text: strip_diff_line_prefix(line).to_owned(),
                        old_line_number: None,
                        new_line_number: Some(new_line),
                    };
                    new_line += 1;
                    parsed
                }
                ProjectGitDiffLineKind::Removed => {
                    let parsed = ProjectGitDiffLine {
                        kind,
                        text: strip_diff_line_prefix(line).to_owned(),
                        old_line_number: Some(old_line),
                        new_line_number: None,
                    };
                    old_line += 1;
                    parsed
                }
            }
        })
        .collect()
}

fn parse_hunk_header(header: &str) -> Result<(u32, u32, u32, u32), String> {
    if header.starts_with("@@@") {
        return Err(format!("unsupported combined diff hunk '{}'", header));
    }
    let ranges = header
        .strip_prefix("@@ ")
        .and_then(|rest| rest.split_once(" @@"))
        .map(|(ranges, _)| ranges)
        .ok_or_else(|| format!("invalid hunk header '{}'", header))?;
    let mut parts = ranges.split_whitespace();
    let old_range = parts
        .next()
        .ok_or_else(|| format!("missing old range in hunk header '{}'", header))?;
    let new_range = parts
        .next()
        .ok_or_else(|| format!("missing new range in hunk header '{}'", header))?;
    if parts.next().is_some() {
        return Err(format!("invalid hunk header '{}'", header));
    }

    let (old_start, old_count) = parse_hunk_range(old_range, '-', header)?;
    let (new_start, new_count) = parse_hunk_range(new_range, '+', header)?;
    Ok((old_start, old_count, new_start, new_count))
}

fn parse_hunk_range(range: &str, prefix: char, header: &str) -> Result<(u32, u32), String> {
    let trimmed = range
        .strip_prefix(prefix)
        .ok_or_else(|| format!("invalid hunk range '{}' in '{}'", range, header))?;
    let (start, count) = match trimmed.split_once(',') {
        Some((start, count)) => (start, count),
        None => (trimmed, "1"),
    };
    let start = start
        .parse::<u32>()
        .map_err(|error| format!("invalid hunk start '{}' in '{}': {error}", range, header))?;
    let count = count
        .parse::<u32>()
        .map_err(|error| format!("invalid hunk count '{}' in '{}': {error}", range, header))?;
    Ok((start, count))
}

fn classify_diff_line(line: &str) -> ProjectGitDiffLineKind {
    match line.chars().next() {
        Some('+') => ProjectGitDiffLineKind::Added,
        Some('-') => ProjectGitDiffLineKind::Removed,
        _ => ProjectGitDiffLineKind::Context,
    }
}

fn strip_diff_line_prefix(line: &str) -> &str {
    line.strip_prefix(['+', '-', ' ']).unwrap_or(line)
}

fn is_git_no_newline_marker(line: &str) -> bool {
    line.starts_with("\\ No newline at end of file")
}
