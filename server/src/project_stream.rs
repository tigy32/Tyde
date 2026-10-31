use crate::process_env::launch::{GroupCommand as Command, ProcessStdio as Stdio};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use notify::{Event, EventKind};

use crate::project_watch::{ProjectWatcher, SharedProjectWatcher};
use protocol::{
    CodeIntelOverviewHeadline, CodeIntelOverviewPayload, CodeIntelOverviewSummary,
    CodeIntelProviderStatus, CodeIntelRootOverview, CodeIntelState, CommandErrorCode,
    CommandErrorPayload, DiffContextMode, FileEntryOp, FrameKind, Project,
    ProjectBinaryFilePayload, ProjectDiffRevision, ProjectDiffScope, ProjectEventPayload,
    ProjectFileContentsPayload, ProjectFileCursor, ProjectFileEntry, ProjectFileKind,
    ProjectFileListPayload, ProjectFileVersion, ProjectFileVersionChange, ProjectFileView,
    ProjectFileViewPayload, ProjectFileViewState, ProjectFileViewStatePayload, ProjectFileWindow,
    ProjectGitChangeKind, ProjectGitCommitSummary, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload, ProjectGitFileStatus,
    ProjectGitStatusPayload, ProjectId, ProjectOpenPathAction, ProjectOpenPathPayload,
    ProjectPartialDir, ProjectPath, ProjectReadDiffPayload, ProjectReadFilePayload,
    ProjectRootGitStatus, ProjectRootListing, ProjectRootPath, ProjectRootStatus,
    ProjectSearchFileResult, ProjectSearchMatch, ProjectSearchPayload, ReviewSummary, StreamPath,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, sleep_until};

use crate::review::state::ReviewCommand;
use crate::review::{ProjectReviews, RegistryCommand, ReviewRegistryHandle};
use crate::stream::Stream;

const PROJECT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);
const PROJECT_GIT_REFRESH_SPACING: Duration = Duration::from_secs(1);
const PROJECT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const GIT_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const RECENT_HISTORY_LIMIT: usize = 100;
pub(crate) const BINARY_PREVIEW_LIMIT_BYTES: u64 = 8 * 1024 * 1024;
const TEXT_READ_LIMIT_BYTES: u64 = (protocol::framing::MAX_LOGICAL_HEADER - 128 * 1024) as u64;
const FILE_READ_LIMIT_BYTES: u64 = TEXT_READ_LIMIT_BYTES + 1;
const BINARY_PREVIEW_ENCODED_LIMIT_BYTES: usize =
    (BINARY_PREVIEW_LIMIT_BYTES as usize).div_ceil(3) * 4;
const BINARY_PREVIEW_PROTOCOL_RESERVE_BYTES: usize = 4 * 1024 * 1024;
const TEXT_PROTOCOL_RESERVE_BYTES: usize = 64 * 1024;
/// A diff answer has to fit one logical protocol frame. Refusing an oversized
/// one here makes it this request's failure; letting it reach the encoder
/// makes it the connection's, because that error ends the writer task and
/// `run_connection` tears down the reader and the application with it. Full
/// context turns a small hunk-mode diff into the whole file, so this is
/// reachable from one click on "expand context".
const DIFF_PAYLOAD_LIMIT_BYTES: usize =
    protocol::framing::MAX_LOGICAL_HEADER - DIFF_PROTOCOL_RESERVE_BYTES;
const DIFF_PROTOCOL_RESERVE_BYTES: usize = 128 * 1024;
const _: () = assert!(
    BINARY_PREVIEW_ENCODED_LIMIT_BYTES + BINARY_PREVIEW_PROTOCOL_RESERVE_BYTES
        < protocol::framing::MAX_LOGICAL_HEADER
);
const _: () = assert!(
    TEXT_READ_LIMIT_BYTES as usize + TEXT_PROTOCOL_RESERVE_BYTES
        < protocol::framing::MAX_LOGICAL_HEADER
);
const _: () = assert!(
    DIFF_PAYLOAD_LIMIT_BYTES + DIFF_PROTOCOL_RESERVE_BYTES <= protocol::framing::MAX_LOGICAL_HEADER
);

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

/// A (relative_path, kind) pair used for comparing file listings between snapshots.
pub(crate) type RawFileEntry = (String, ProjectFileKind);

/// One root's scan result. An unreadable root is reported per root instead of
/// failing the scan, so the project's other roots keep loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootScan {
    Available(RootEntries),
    Unavailable(String),
}

/// A readable root's listing, plus the listed directories whose children were
/// cut off at the bound and how many of them were left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RootEntries {
    entries: BTreeSet<RawFileEntry>,
    partial: BTreeMap<String, u32>,
}

impl RootEntries {
    fn add(&mut self, dir: &str, listing: Listing) {
        self.entries.extend(listing.entries);
        if listing.omitted > 0 {
            self.partial.insert(
                dir.to_owned(),
                u32::try_from(listing.omitted).unwrap_or(u32::MAX),
            );
        }
    }
}

pub(crate) struct FileScan {
    roots: BTreeMap<ProjectRootPath, RootScan>,
}

struct FileViewRead {
    view: ProjectFileView,
    generation: u64,
    last_read: u64,
    dirty: bool,
    refresh_pending: bool,
    cancel: Arc<AtomicBool>,
}

impl Drop for FileViewRead {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitAccessMode {
    ReadOnly,
    Mutating,
}

#[derive(Debug)]
pub(crate) struct ProjectSnapshotState {
    /// Previous file entries per root, used to decide whether a new full snapshot is needed.
    pub file_entries: BTreeMap<ProjectRootPath, RootScan>,
    pub git_status: Option<Value>,
    diff_context_modes: HashMap<(StreamPath, ProjectDiffRequestKey), RememberedDiffContext>,
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
    pub task: JoinHandle<Result<(), String>>,
    pub handle: ProjectStreamHandle,
}

impl Drop for ProjectStreamSubscription {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
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
    views: HashMap<Option<ProjectPath>, FileViewRead>,
}

struct PendingProjectSubscriber {
    host_path: StreamPath,
    subscriber: ProjectSubscriber,
    reply: oneshot::Sender<Result<(), String>>,
}

fn publish_subscriber(
    pending: PendingProjectSubscriber,
    project: &Project,
    snapshot: &mut ProjectSnapshotState,
    reviews: Vec<ReviewSummary>,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    warnings: [(&str, &Option<String>); 3],
) {
    let result = emit_snapshot_to_stream(
        &pending.subscriber.stream,
        project,
        snapshot,
        reviews,
        pending.subscriber.file_delivery,
    );
    if result.is_ok() {
        for (operation, warning) in warnings {
            if let Some(message) = warning {
                emit_project_command_error(
                    &pending.subscriber.stream,
                    FrameKind::ProjectFileList,
                    operation,
                    message.clone(),
                    false,
                );
            }
        }
        subscribers.insert(pending.host_path, pending.subscriber);
    } else {
        subscribers.remove(&pending.host_path);
        snapshot
            .diff_context_modes
            .retain(|(subscriber, _), _| subscriber != &pending.host_path);
    }
    let _ = pending.reply.send(result);
}

#[derive(Clone)]
pub(crate) struct ProjectStreamHandle {
    tx: mpsc::UnboundedSender<ProjectStreamCommand>,
    catalog: watch::Sender<protocol::ProjectCatalogRecord>,
}

impl ProjectStreamHandle {
    pub(crate) fn execution_project(&self) -> Project {
        self.catalog.borrow().project.clone()
    }
}

enum ProjectStreamCommand {
    Shutdown,
    ReviewRegistry(RegistryCommand),
    EmitReviewListChanged,
    AddSubscriber {
        host_path: StreamPath,
        stream: Stream,
        file_delivery: ProjectFileDelivery,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveSubscriber {
        host_path: StreamPath,
    },
    FileView {
        payload: ProjectFileViewPayload,
        requester: StreamPath,
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
    pub(crate) fn shutdown(&self) {
        let _ = self.tx.send(ProjectStreamCommand::Shutdown);
    }

    pub(crate) fn adopt_catalog(&self, generation: u64, project: Project) {
        self.catalog.send_if_modified(|record| {
            if generation <= record.generation {
                return false;
            }
            *record = protocol::ProjectCatalogRecord {
                generation,
                project,
            };
            true
        });
    }

    pub(crate) fn route_review_registry(&self, command: RegistryCommand) -> Result<(), String> {
        self.tx
            .send(ProjectStreamCommand::ReviewRegistry(command))
            .map_err(|_| "project review owner stopped".to_owned())
    }

    pub(crate) fn emit_review_list_changed(&self) -> Result<(), String> {
        self.tx
            .send(ProjectStreamCommand::EmitReviewListChanged)
            .map_err(|_| "project review owner stopped".to_owned())
    }

    pub(crate) fn begin_add_subscriber(
        &self,
        host_path: StreamPath,
        stream: Stream,
        file_delivery: ProjectFileDelivery,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::AddSubscriber {
                host_path,
                stream,
                file_delivery,
                reply,
            })
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        Ok(response)
    }

    pub(crate) async fn remove_subscriber(&self, host_path: StreamPath) {
        let _ = self
            .tx
            .send(ProjectStreamCommand::RemoveSubscriber { host_path });
    }

    pub(crate) fn set_file_view(
        &self,
        payload: ProjectFileViewPayload,
        requester: StreamPath,
    ) -> Result<(), String> {
        self.tx
            .send(ProjectStreamCommand::FileView { payload, requester })
            .map_err(|_| "project stream subscription stopped".to_owned())
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

pub(crate) fn spawn_project_subscription(
    shared_watcher: SharedProjectWatcher,
    generation: u64,
    project: Project,
    review_registry: ReviewRegistryHandle,
    force_watch_limit: bool,
) -> Result<ProjectStreamSubscription, String> {
    let (watch_tx, watch_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (review_tx, review_rx) = mpsc::unbounded_channel();
    let initial_catalog = protocol::ProjectCatalogRecord {
        generation,
        project: project.clone(),
    };
    let (catalog, catalog_rx) = watch::channel(initial_catalog.clone());
    let handle = ProjectStreamHandle {
        tx: command_tx,
        catalog,
    };
    review_registry.register_project(project.id.clone(), handle.clone())?;
    let snapshot = ProjectSnapshotState {
        file_entries: BTreeMap::new(),
        git_status: None,
        diff_context_modes: HashMap::new(),
        code_intel_overview: initial_code_intel_overview(project.root_paths()),
    };
    let task = spawn_project_task(async move {
        run_project_subscription(
            shared_watcher,
            catalog_rx,
            project.id.clone(),
            initial_catalog,
            snapshot,
            None,
            Vec::new(),
            watch_tx,
            watch_rx,
            force_watch_limit,
            command_rx,
            review_registry,
            review_tx,
            review_rx,
        )
        .await
    })?;
    Ok(ProjectStreamSubscription { task, handle })
}

pub(crate) fn spawn_project_task<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<JoinHandle<T>, String> {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        return Ok(runtime.spawn(future));
    }
    let (task_tx, task_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("tyde-project".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = task_tx.send(Err(format!("create project runtime: {error}")));
                    return;
                }
            };
            let (done_tx, done_rx) = oneshot::channel();
            let task = runtime.spawn(async move {
                let result = future.await;
                let _ = done_tx.send(());
                result
            });
            if task_tx.send(Ok(task)).is_err() {
                return;
            }
            runtime.block_on(async {
                let _ = done_rx.await;
            });
        })
        .map_err(|error| format!("spawn project runtime thread: {error}"))?;
    task_rx
        .recv()
        .map_err(|error| format!("receive project task: {error}"))?
}

fn start_project_watcher(
    shared_watcher: SharedProjectWatcher,
    roots: Vec<ProjectRootPath>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    force_watch_limit: bool,
) -> mpsc::UnboundedReceiver<Result<ProjectWatcher, ProjectWatcherFailure>> {
    let (watcher_ready_tx, watcher_ready_rx) = mpsc::unbounded_channel();
    {
        let watch_tx = watch_tx.clone();
        let tx = watcher_ready_tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("tyde-project-watch-init".to_owned())
            .spawn(move || {
                let result =
                    create_project_watcher(shared_watcher, &roots, watch_tx, force_watch_limit);
                let _ = tx.send(result);
            })
        {
            let _ = watcher_ready_tx.send(Err(ProjectWatcherFailure {
                message: format!("failed to spawn project watcher initialization thread: {error}"),
                limit_reached: false,
            }));
        }
    }
    watcher_ready_rx
}

fn create_project_watcher(
    shared_watcher: SharedProjectWatcher,
    roots: &[ProjectRootPath],
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    force_watch_limit: bool,
) -> Result<ProjectWatcher, ProjectWatcherFailure> {
    if force_watch_limit && let Some(root) = roots.first() {
        return Err(ProjectWatcherFailure::forced_limit(root));
    }
    tracing::debug!(
        roots = roots.len(),
        "registering project filesystem watches"
    );
    ProjectWatcher::new(shared_watcher, roots, watch_tx).map_err(|error| {
        ProjectWatcherFailure::from_notify(
            format!("failed to create project filesystem watcher for roots {roots:?}"),
            error,
        )
    })
}

/// Roots the watcher can register: unavailable roots are listed with their
/// status instead, and picked up again by watcher initialization once they
/// reappear.
fn watchable_roots(project: &Project) -> Vec<ProjectRootPath> {
    project
        .root_paths()
        .into_iter()
        .filter(|root| root_unavailable_reason(root).is_none())
        .collect()
}

async fn background_project_read<T: Send + 'static>(
    project: &Project,
    operation: &'static str,
    read: impl FnOnce(&Project) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let queued = std::time::Instant::now();
    let project = project.clone();
    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let queue_ms = queued.elapsed().as_millis();
        let result = read(&project);
        tracing::debug!(project_id = %project.id, operation, queue_ms,
            elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(),
            "completed background project read");
        result
    })
    .await
    .map_err(|error| format!("project {operation} task failed: {error}"))?
}

fn initialize_snapshot(project: &Project) -> Result<(ProjectSnapshotState, FileScan), String> {
    let mut scan = scan_project_files(project)?;
    let mut snapshot = ProjectSnapshotState {
        file_entries: std::mem::take(&mut scan.roots),
        git_status: None,
        diff_context_modes: HashMap::new(),
        code_intel_overview: initial_code_intel_overview(project.root_paths()),
    };
    let git_status = build_git_status(project)?;
    snapshot.git_status = Some(serialize_git_status(&git_status)?);
    Ok((snapshot, scan))
}

#[allow(clippy::too_many_arguments)]
async fn run_project_subscription(
    shared_watcher: SharedProjectWatcher,
    mut catalog: watch::Receiver<protocol::ProjectCatalogRecord>,
    project_id: ProjectId,
    initial_catalog: protocol::ProjectCatalogRecord,
    mut snapshot: ProjectSnapshotState,
    mut watcher: Option<ProjectWatcher>,
    mut watched_roots: Vec<ProjectRootPath>,
    mut watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    mut watch_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    force_watch_limit: bool,
    mut command_rx: mpsc::UnboundedReceiver<ProjectStreamCommand>,
    review_registry: ReviewRegistryHandle,
    review_tx: mpsc::UnboundedSender<(protocol::ReviewId, ReviewCommand)>,
    mut review_rx: mpsc::UnboundedReceiver<(protocol::ReviewId, ReviewCommand)>,
) -> Result<(), String> {
    let mut project = initial_catalog.project;
    let mut catalog_generation = initial_catalog.generation;
    #[cfg(feature = "test-support")]
    if let Err(error) = background_project_read(&project, "actor startup", |project| {
        for root in project.root_paths() {
            scan_test_support::run(
                Path::new(&root.0),
                scan_test_support::ScanPoint::ActorStarting,
            );
        }
        Ok(())
    })
    .await
    {
        tracing::error!(%error, "project startup barrier failed");
        return Err(error);
    }
    let mut reviews = match ProjectReviews::load(&project, review_registry, review_tx).await {
        Ok(reviews) => reviews,
        Err(error) => {
            tracing::error!(%project_id, %error, "failed to load project review state");
            return Err(error);
        }
    };
    let mut snapshot_initialized = false;
    let mut emit_reviews = false;
    let mut subscribers = HashMap::<StreamPath, ProjectSubscriber>::new();
    // Centralized per-file version counter. The single bump point
    // (`bump_file_version`) is funneled here from every source: file reads,
    // filesystem-watcher changes, and (transitively, via the watcher) agent
    // writes. See `dev-docs/24-code-intelligence.md` §2.4.
    let mut file_versions = HashMap::<ProjectPath, ProjectFileVersion>::new();
    let mut observed_files = HashSet::<ProjectPath>::new();
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
    let (_, mut watcher_ready_rx) = mpsc::unbounded_channel();
    let mut watcher_initializing = false;
    let mut watcher_retry = Instant::now();
    let mut watcher_warning = None::<String>;
    let mut file_warning = None::<String>;
    let mut git_warning = None::<String>;
    let mut debounce_active = false;
    let mut next_git_refresh = Instant::now();
    let mut pending_git = PendingGitRefresh::default();
    let mut diff_request_generation = 0_u64;
    let mut git_tasks = JoinSet::new();
    let mut file_tasks = JoinSet::new();
    let mut initialization_tasks = JoinSet::new();
    let mut pending_subscribers = Vec::new();
    let mut pending_files = PendingFileRefresh::default();
    let mut file_view_tasks = JoinSet::new();
    let mut view_watch_tasks = JoinSet::new();
    let mut observation_tasks = JoinSet::new();
    let mut view_watch_generation = 0_u64;
    let mut view_watch_pending = false;
    let mut view_watch_retry = Instant::now();
    let mut view_generation = 0_u64;
    let mut view_read_ordinal = 0_u64;
    let mut debounce_sleep = Box::pin(sleep(Duration::from_secs(60 * 60 * 24 * 365)));
    let mut git_poll = interval_at(
        Instant::now() + GIT_STATUS_POLL_INTERVAL,
        GIT_STATUS_POLL_INTERVAL,
    );
    git_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        let record = catalog.borrow_and_update().clone();
        if record.generation > catalog_generation {
            catalog_generation = record.generation;
            let roots_changed = project.root_paths() != record.project.root_paths();
            project = record.project;
            if roots_changed {
                snapshot_initialized = false;
                snapshot
                    .file_entries
                    .retain(|root, _| project.root_paths().contains(root));
                invalidate_file_views(&mut subscribers, None, &watcher_roots);
                snapshot
                    .diff_context_modes
                    .retain(|(_, key), _| project.root_paths().contains(&key.root));
                observed_files.retain(|path| project.root_paths().contains(&path.root));
                watcher = None;
                watcher_initializing = false;
                watcher_retry = Instant::now();
                pending_files.request(true);
                pending_git.request(true, true);
                file_versions.retain(|path, _| project.root_paths().contains(&path.root));
                pending_file_version_changes
                    .retain(|path, _| project.root_paths().contains(&path.root));
                tracing::info!(target: "tyde::project_operation", generation = catalog_generation, root_count = project.root_paths().len(), stage = "metadata_adopt", "project authorization adopted before derived refresh");
            }
        }
        if !pending_subscribers.is_empty() && initialization_tasks.is_empty() {
            let record = catalog.borrow().clone();
            initialization_tasks.spawn(async move {
                let initial =
                    background_project_read(&record.project, "initialize", move |project| {
                        initialize_snapshot(project)
                    })
                    .await;
                (record.generation, initial)
            });
        }
        if pending_files.requested && file_tasks.is_empty() {
            let request = std::mem::take(&mut pending_files);
            let project = project.clone();
            let generation = catalog_generation;
            file_tasks.spawn(async move {
                let result = background_project_read(&project, "files", move |project| {
                    scan_project_files(project)
                })
                .await;
                CompletedFileRefresh {
                    generation,
                    request,
                    result,
                }
            });
        }
        if let Some(watcher) = watcher.as_mut()
            && Instant::now() >= view_watch_retry
        {
            let directories = subscribers
                .values()
                .flat_map(|subscriber| subscriber.views.keys().flatten())
                .filter(|path| validate_directory_view(&project, path).is_ok())
                .cloned()
                .collect();
            let queries = subscribers
                .values()
                .flat_map(|subscriber| subscriber.views.values())
                .flat_map(|read| match &read.view {
                    ProjectFileView::QuickOpen { .. } => project
                        .root_paths()
                        .into_iter()
                        .map(|root| ProjectPath {
                            root,
                            relative_path: String::new(),
                        })
                        .collect::<Vec<_>>(),
                    ProjectFileView::Directory { path, filter, .. } if !filter.is_empty() => {
                        vec![path.clone()]
                    }
                    ProjectFileView::Directory { .. } => Vec::new(),
                })
                .filter(|path| validate_directory_view(&project, path).is_ok())
                .collect();
            match watcher.set_view_directories(directories, queries) {
                Ok(Some(update)) => {
                    view_watch_generation += 1;
                    let generation = view_watch_generation;
                    view_watch_pending = true;
                    view_watch_tasks.spawn(async move {
                        let result = update
                            .response
                            .await
                            .map_err(|_| {
                                notify::Error::generic("project filesystem watcher stopped")
                            })
                            .and_then(|result| result);
                        (generation, update.added, update.cancel, result)
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    view_watch_retry = Instant::now() + PROJECT_RETRY_INTERVAL;
                    warn_project_retry(
                        &project_id,
                        &mut subscribers,
                        "project_watch",
                        error.to_string(),
                        &mut watcher_warning,
                    )
                    .await;
                }
            }
        }
        if file_view_tasks.is_empty() && !view_watch_pending {
            let next = subscribers
                .iter_mut()
                .flat_map(|(requester, subscriber)| {
                    subscriber
                        .views
                        .iter_mut()
                        .map(move |(key, read)| (requester.clone(), key.clone(), read))
                })
                .filter(|(_, _, read)| read.dirty)
                .min_by_key(|(_, _, read)| read.last_read)
                .map(|(requester, key, read)| {
                    view_read_ordinal += 1;
                    read.last_read = view_read_ordinal;
                    read.dirty = false;
                    (
                        requester,
                        key,
                        read.generation,
                        read.view.clone(),
                        read.cancel.clone(),
                    )
                });
            if let Some((requester, key, generation, view, cancel)) = next {
                let retained = retained_directory_children(&subscribers[&requester], &key);
                let retained_for_read = retained.clone();
                let project = project.clone();
                let view_catalog_generation = catalog_generation;
                file_view_tasks.spawn(async move {
                    let result = background_project_read(&project, "file view", move |project| {
                        read_file_view(project, view, &retained_for_read, &cancel)
                    })
                    .await;
                    (
                        view_catalog_generation,
                        requester,
                        key,
                        generation,
                        retained,
                        result,
                    )
                });
            }
        }
        if reviews.take_changed() || emit_reviews {
            emit_reviews = false;
            let payload = ProjectEventPayload::ReviewListChanged {
                reviews: reviews.summaries(),
            };
            if let Err(error) = fan_out_payload(&mut subscribers, FrameKind::ProjectEvent, &payload)
            {
                tracing::warn!(%project_id, %error, "failed to emit project review summaries");
            }
        }
        #[cfg(feature = "test-support")]
        if !debounce_active
            && !pending_files.requested
            && file_tasks.is_empty()
            && watch_rx.is_empty()
        {
            for root in project.root_paths() {
                scan_test_support::run(
                    Path::new(&root.0),
                    scan_test_support::ScanPoint::FileRefreshSettled,
                );
            }
        }
        tokio::select! {
            biased;
            changed = catalog.changed() => { if changed.is_err() { break; } }
            Some((review_id, command)) = review_rx.recv() => {
                reviews.handle_command(&project, review_id, command).await;
            }
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    break;
                };
                let command_project = catalog.borrow().project.clone();
                match command {
                    ProjectStreamCommand::Shutdown => break,
                    ProjectStreamCommand::ReviewRegistry(command) => {
                        reviews.handle_registry(&command_project, command).await;
                    }
                    ProjectStreamCommand::EmitReviewListChanged => { emit_reviews = true; }
                    ProjectStreamCommand::AddSubscriber { host_path, stream, file_delivery, reply } => {
                        let pending = PendingProjectSubscriber { host_path, subscriber: ProjectSubscriber { stream, file_delivery, views: HashMap::new() }, reply };
                        let current = catalog.borrow();
                        if snapshot_initialized && current.generation == catalog_generation {
                            publish_subscriber(pending, &current.project, &mut snapshot, reviews.summaries(), &mut subscribers,
                                [("project_watch", &watcher_warning), ("project_watch", &file_warning), ("project_git_status", &git_warning)]);
                        } else {
                            pending_subscribers.push(pending);
                        }
                    }
                    ProjectStreamCommand::RemoveSubscriber { host_path } => {
                        pending_subscribers.retain(|pending| pending.host_path != host_path);
                        subscribers.remove(&host_path);
                        snapshot.diff_context_modes.retain(|(subscriber, _), _| subscriber != &host_path);
                    }
                    ProjectStreamCommand::FileView { payload, requester } => {
                        view_watch_retry = Instant::now();
                        if let Some(subscriber) = subscribers.get_mut(&requester) {
                            let key = file_view_key(&payload.view);
                            if !payload.active {
                                if subscriber.views.get(&key).is_none_or(|read| read.view != payload.view) { continue; }
                                subscriber.views.remove(&key);
                                if let Some(path) = &key && let Some(parent) = Path::new(&path.relative_path).parent() {
                                    let parent_key = Some(ProjectPath { root: path.root.clone(), relative_path: parent.to_string_lossy().into_owned() });
                                    if let Some(read) = subscriber.views.get_mut(&parent_key) { read.dirty = true; }
                                }
                                let state = ProjectFileViewStatePayload { view: payload.view, state: ProjectFileViewState::Closed };
                                send_file_view_state(&mut subscribers, &requester, state);
                            } else {
                                view_generation += 1;
                                let view = payload.view;
                                let read = FileViewRead { view: view.clone(), generation: view_generation, last_read: 0,
                                    dirty: true, refresh_pending: false, cancel: Arc::new(AtomicBool::new(false)) };
                                if let Some(path) = &key && let Some(parent) = Path::new(&path.relative_path).parent() {
                                    let parent_key = Some(ProjectPath { root: path.root.clone(), relative_path: parent.to_string_lossy().into_owned() });
                                    if let Some(parent_read) = subscriber.views.get_mut(&parent_key) { parent_read.dirty = true; }
                                }
                                subscriber.views.insert(key, read);
                                let state = ProjectFileViewStatePayload { view, state: ProjectFileViewState::Loading };
                                send_file_view_state(&mut subscribers, &requester, state);
                            }
                        }
                    }
                    ProjectStreamCommand::Refresh { reply } => {
                        invalidate_file_views(&mut subscribers, None, &watcher_roots);
                        pending_files.request(true);
                        pending_files.replies.push(reply);
                    }
                    ProjectStreamCommand::RememberDiffContext { host_path, key, context_mode, reply } => {
                        diff_request_generation += 1;
                        snapshot.diff_context_modes.insert((host_path, key), RememberedDiffContext { context_mode, generation: diff_request_generation });
                        let _ = reply.send(Ok(()));
                    }
                    ProjectStreamCommand::UpdateCodeIntelProviderStatus { root, status } => {
                        let provider = status.provider.clone();
                        let language = status.language.clone();
                        let state = status.state;
                        let resource_mode = status.resource_mode;
                        if update_code_intel_provider_status(
                            &mut snapshot.code_intel_overview,
                            &command_project,
                            root.clone(),
                            status,
                        ) && let Err(error) = fan_out_payload(
                            &mut subscribers,
                            FrameKind::CodeIntelOverview,
                            &snapshot.code_intel_overview,
                        )
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
                        let result = read_file(&command_project, payload).map(|mut contents| {
                            contents.version = file_versions
                                .get(&contents.path)
                                .copied()
                                .unwrap_or(ProjectFileVersion(0));
                            contents
                        });
                        let observation = if let Ok(contents) = &result {
                            observed_files.insert(contents.path.clone());
                            match watcher.as_mut() { Some(watcher) => watcher.observe(&contents.path).map(Some), None => Ok(None) }
                        } else { Ok(None) };
                        #[cfg(feature = "test-support")]
                        if matches!(&observation, Ok(Some(_))) && let Ok(contents) = &result {
                            scan_test_support::run(&Path::new(&contents.path.root.0).join(&contents.path.relative_path), scan_test_support::ScanPoint::FileObservationWaiting);
                        }
                        complete_after_file_watch(&mut observation_tasks, watch_tx.clone(), observation, move || { let _ = reply.send(result); });
                    }
                    ProjectStreamCommand::CurrentFileVersion { path, reply } => {
                        let result = current_file_version(&command_project, &path, &file_versions);
                        // Later code-intel subscriptions share the listener,
                        // but each still holds its own file version.
                        let observation = if result.is_ok() {
                            observed_files.insert(path.clone());
                            match watcher.as_mut() { Some(watcher) => watcher.observe(&path).map(Some), None => Ok(None) }
                        } else { Ok(None) };
                        complete_after_file_watch(&mut observation_tasks, watch_tx.clone(), observation, move || { let _ = reply.send(result); });
                    }
                    ProjectStreamCommand::ValidatePath { path, reply } => {
                        let _ = reply.send(validate_project_path(&command_project, &path));
                    }
                    ProjectStreamCommand::RegisterFileVersionListenerAndCurrentVersion {
                        path,
                        listener,
                        reply,
                    } => {
                        let observation = if validate_project_path(&command_project, &path).is_ok() {
                            observed_files.insert(path.clone());
                            match watcher.as_mut() { Some(watcher) => watcher.observe(&path).map(Some), None => Ok(None) }
                        } else { Ok(None) };
                        let result = register_file_version_listener_and_current_version(
                            &command_project,
                            path,
                            &file_versions,
                            &mut file_version_listeners,
                            listener,
                        );
                        complete_after_file_watch(&mut observation_tasks, watch_tx.clone(), observation, move || { let _ = reply.send(result); });
                    }
                }
            }
            _ = sleep_until(watcher_retry), if watcher.is_none() && !watcher_initializing && !subscribers.is_empty() => {
                tracing::debug!(%project_id, "retrying project filesystem watcher initialization");
                watched_roots = watchable_roots(&project);
                // Retired watchers and their pending observations must not
                // deliver errors into the replacement watcher's mailbox.
                (watch_tx, watch_rx) = mpsc::unbounded_channel();
                watcher_ready_rx = start_project_watcher(shared_watcher.clone(), watched_roots.clone(), watch_tx.clone(), force_watch_limit);
                watcher_initializing = true;
            }
            maybe_watcher = watcher_ready_rx.recv(), if watcher_initializing => {
                watcher_initializing = false;
                watcher_retry = Instant::now() + PROJECT_RETRY_INTERVAL;
                match maybe_watcher {
                    Some(Ok(mut ready_watcher)) => {
                        for path in observed_files.iter().filter(|path| watched_roots.contains(&path.root)) {
                            let observation = ready_watcher.observe(path).map(Some);
                            complete_after_file_watch(&mut observation_tasks, watch_tx.clone(), observation, || {});
                        }
                        watcher = Some(ready_watcher);
                        invalidate_file_views(&mut subscribers, None, &watcher_roots);
                        if watcher_warning.take().is_some() {
                            tracing::info!(%project_id, observed_files = observed_files.len(), "project filesystem watching recovered");
                        }
                        // Every adoption closes a watch gap and discards retired
                        // content events, including root changes with no warning.
                        for path in &observed_files {
                            let version = bump_file_version(&mut file_versions, path);
                            pending_file_version_changes.insert(path.clone(), version);
                        }
                        tracing::debug!(%project_id, watched_roots = watched_roots.len(), observed_files = observed_files.len(), deferred_observations = observed_files.iter().filter(|path| !watched_roots.contains(&path.root)).count(), "project filesystem watcher adopted; observed versions invalidated");
                        #[cfg(feature = "test-support")]
                        for root in &watched_roots {
                            scan_test_support::run(Path::new(&root.0), scan_test_support::ScanPoint::WatcherAdopted);
                        }
                        pending_update.merge(PendingProjectUpdate { files: true, git: true });
                        debounce_active = true;
                        debounce_sleep.as_mut().reset(Instant::now() + PROJECT_REFRESH_DEBOUNCE);
                    }
                    result => {
                        let message = match result {
                            Some(Err(error)) if error.limit_reached => project_watch_limit_guidance(&error.message),
                            Some(Err(error)) => error.message,
                            None => "project filesystem watcher failed to initialize".to_owned(),
                            Some(Ok(_)) => unreachable!(),
                        };
                        warn_project_retry(&project_id, &mut subscribers, "project_watch", message, &mut watcher_warning).await;
                    }
                }
            }
            maybe_event = watch_rx.recv(), if watcher.is_some() => {
                let event_result = maybe_event.unwrap_or_else(|| Err(notify::Error::generic("project filesystem watcher stopped unexpectedly")));

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
                        let views_changed = invalidate_file_views(&mut subscribers, Some(&event), &watcher_roots);
                        let refresh = classify_watch_event(&event);
                        if views_changed || !refresh.is_empty() || !pending_file_version_changes.is_empty() {
                            pending_update.merge(refresh);
                            tracing::debug!(
                                project_id = %project_id,
                                event_kind = ?event.kind,
                                pending_files = pending_file_version_changes.len(),
                                debounce_active,
                                "scheduling project filesystem refresh"
                            );
                            // Later activity must not postpone already queued changes.
                            if !debounce_active {
                                debounce_active = true;
                                debounce_sleep.as_mut().reset(Instant::now() + PROJECT_REFRESH_DEBOUNCE);
                            }
                        }
                    }
                    Err(error) => {
                        watcher = None;
                        if watched_roots != watchable_roots(&project) {
                            // A watched root vanished or returned; the file listing reports it.
                            tracing::info!(%project_id, %error, "project root availability changed; re-registering watcher");
                            watcher_retry = Instant::now();
                            pending_update.merge(PendingProjectUpdate { files: true, git: true });
                            if !debounce_active {
                                debounce_active = true;
                                debounce_sleep.as_mut().reset(Instant::now() + PROJECT_REFRESH_DEBOUNCE);
                            }
                            continue;
                        }
                        let message = format!("project filesystem watcher failed: {error}");
                        let message = if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
                            project_watch_limit_guidance(&message)
                        } else {
                            message
                        };
                        watcher_retry = Instant::now() + PROJECT_RETRY_INTERVAL;
                        warn_project_retry(&project_id, &mut subscribers, "project_watch", message, &mut watcher_warning).await;
                    }
                }
            }
            _ = &mut debounce_sleep, if debounce_active => {
                debounce_active = false;
                for subscriber in subscribers.values_mut() {
                    for read in subscriber.views.values_mut() {
                        read.dirty |= std::mem::take(&mut read.refresh_pending);
                    }
                }
                let refresh = pending_update.take();
                let full = watcher.is_none();
                if refresh.files || full { pending_files.request(full); }
                if refresh.git || full {
                    pending_git.request(full, refresh.files || full);
                }
                let changes = take_pending_file_version_changes(&mut pending_file_version_changes);
                notify_file_version_listeners(&mut file_version_listeners, &changes);
                // Directory reconciliation can include the entire inventory.
                // Code-intel needs all workspace changes; frontend invalidation
                // only needs versions held by readers and file subscribers.
                let files: Vec<_> = changes
                    .iter()
                    .filter(|change| observed_files.contains(&change.path))
                    .map(|change| ProjectFileVersionChange {
                        path: change.path.clone(),
                        version: change.version,
                    })
                    .collect();
                if !files.is_empty()
                    && let Err(error) = broadcast_project_event(
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
            _ = git_poll.tick(), if !subscribers.is_empty() && !debounce_active && git_tasks.is_empty() && !pending_git.requested => {
                pending_git.request(git_warning.is_some(), git_warning.is_some());
            }
            _ = sleep_until(if pending_git.full {
                Instant::now()
            } else {
                next_git_refresh
            }), if pending_git.requested && git_tasks.is_empty() => {
                let request = std::mem::take(&mut pending_git);
                tracing::debug!(%project_id, full = request.full, refresh_diffs = request.refresh_diffs,
                    waiting_replies = request.replies.len(), "starting background project Git refresh");
                let project = project.clone();
                let previous_status = snapshot.git_status.clone();
                let remembered = snapshot.diff_context_modes.clone();
                let generation = catalog_generation;
                git_tasks.spawn(async move {
                    let refresh_diffs = request.refresh_diffs;
                    let result = background_project_read(&project, "git refresh", move |project| {
                        read_git_refresh(project, previous_status, remembered, refresh_diffs)
                    }).await;
                    CompletedGitRefresh { generation, project, request, result }
                });
            }
            completed = initialization_tasks.join_next(), if !initialization_tasks.is_empty() => {
                let (generation, result) = match completed {
                    Some(Ok(completed)) => completed,
                    Some(Err(error)) => (catalog_generation, Err(format!("project bootstrap worker failed: {error}"))),
                    None => continue,
                };
                let current = catalog.borrow();
                if generation != current.generation { continue; }
                match result {
                    Ok((initial, _scan)) => {
                        snapshot = initial;
                        snapshot_initialized = true;
                        for pending in pending_subscribers.drain(..) {
                            publish_subscriber(pending, &current.project, &mut snapshot, reviews.summaries(), &mut subscribers,
                                [("project_watch", &watcher_warning), ("project_watch", &file_warning), ("project_git_status", &git_warning)]);
                        }
                    }
                    Err(error) => {
                        for pending in pending_subscribers.drain(..) { let _ = pending.reply.send(Err(error.clone())); }
                    }
                }
            }
            completed = file_tasks.join_next(), if !file_tasks.is_empty() => {
                let completed = match completed {
                    Some(Ok(completed)) => completed,
                    Some(Err(error)) => {
                        warn_project_retry(&project_id, &mut subscribers, "project_files", error.to_string(), &mut file_warning).await;
                        continue;
                    }
                    None => continue,
                };
                if completed.generation != catalog_generation {
                    pending_files.request(completed.request.full);
                    pending_files.replies.extend(completed.request.replies);
                    continue;
                }
                match completed.result {
                    Ok(scan) => {
                        let publication = {
                            let current = catalog.borrow();
                            if completed.generation != current.generation {
                                pending_files.request(completed.request.full);
                                pending_files.replies.extend(completed.request.replies);
                                continue;
                            }
                            let changed = snapshot.file_entries != scan.roots;
                            snapshot.file_entries = scan.roots;
                            (|| -> Result<(), String> {
                                if changed || completed.request.full {
                                    let payload = full_file_list_from_raw(&current.project, &snapshot.file_entries);
                                    fan_out_payload(&mut subscribers, FrameKind::ProjectFileList, &payload)?;
                                }
                                if sync_code_intel_overview_roots(&mut snapshot.code_intel_overview, current.project.root_paths()) {
                                    fan_out_payload(&mut subscribers, FrameKind::CodeIntelOverview, &snapshot.code_intel_overview)?;
                                }
                                Ok(())
                            })()
                        };
                        if let Err(error) = publication {
                        warn_project_retry(&project_id, &mut subscribers, "project_files", error, &mut file_warning).await;
                        } else if file_warning.take().is_some() { tracing::info!(%project_id, "project file refresh recovered"); }
                        pending_git.request(completed.request.full, true);
                        pending_git.replies.extend(completed.request.replies);
                    }
                    Err(error) => {
                        for reply in completed.request.replies { let _ = reply.send(Err(error.clone())); }
                        warn_project_retry(&project_id, &mut subscribers, "project_files", error, &mut file_warning).await;
                        pending_update.merge(PendingProjectUpdate { files: true, git: true });
                        debounce_active = true;
                        debounce_sleep.as_mut().reset(Instant::now() + PROJECT_RETRY_INTERVAL);
                    }
                }
            }
            _ = sleep_until(view_watch_retry), if Instant::now() < view_watch_retry && watcher.is_some() => {}
            completed = observation_tasks.join_next(), if !observation_tasks.is_empty() => {
                if let Some(Err(error)) = completed { return Err(format!("file observation task failed: {error}")); }
            }
            completed = view_watch_tasks.join_next(), if !view_watch_tasks.is_empty() => {
                let Some(Ok((generation, added, cancel, result))) = completed else { return Err("file view watch completion failed".into()); };
                if generation != view_watch_generation { continue; }
                view_watch_pending = false;
                if cancel.load(Ordering::Relaxed) { continue; }
                match result {
                    Ok(()) => {
                        if watcher_warning.take().is_some() { tracing::info!(%project_id, "active file view watches recovered"); }
                        for subscriber in subscribers.values_mut() {
                            for (key, read) in &mut subscriber.views {
                                if key.as_ref().is_some_and(|path| added.contains(path)) { read.dirty = true; }
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(watcher) = &mut watcher { watcher.forget_view_directories(); }
                        view_watch_retry = Instant::now() + PROJECT_RETRY_INTERVAL;
                        for subscriber in subscribers.values_mut() {
                            for read in subscriber.views.values_mut() { read.dirty = true; }
                        }
                        warn_project_retry(&project_id, &mut subscribers, "project_watch", error.to_string(), &mut watcher_warning).await;
                    }
                }
            }
            completed = file_view_tasks.join_next(), if !file_view_tasks.is_empty() => {
                match completed {
                    Some(Ok((generation_catalog, requester, key, generation, retained, result))) => {
                        if let Some(subscriber) = subscribers.get_mut(&requester) {
                            let current_retained = retained_directory_children(subscriber, &key);
                            let Some(read) = subscriber.views.get_mut(&key) else { continue; };
                            if read.generation != generation { continue; }
                            if generation_catalog != catalog.borrow().generation || retained != current_retained {
                                read.dirty = true;
                                continue;
                            }
                            let state = match result {
                                Ok(state) => state,
                                Err(message) => ProjectFileViewState::Unavailable { message },
                            };
                            let state = ProjectFileViewStatePayload { view: read.view.clone(), state };
                            send_file_view_state(&mut subscribers, &requester, state);
                        }
                    }
                    Some(Err(error)) => return Err(format!("file view worker failed: {error}")),
                    None => {}
                }
            }
            completed = git_tasks.join_next(), if !git_tasks.is_empty() => {
                next_git_refresh = Instant::now() + PROJECT_GIT_REFRESH_SPACING;
                git_poll.reset();
                let mut completed = match completed.expect("active Git refresh") {
                    Ok(completed) => completed,
                    Err(error) => {
                        let message = format!("project Git refresh task failed: {error}");
                        warn_project_retry(&project_id, &mut subscribers, "project_git_status", message, &mut git_warning).await;
                        continue;
                    }
                };
                if completed.generation != catalog_generation || completed.project.root_paths() != project.root_paths() {
                    tracing::debug!(%project_id, "discarding Git refresh for obsolete project roots");
                    pending_git.request(true, true);
                    pending_git.replies.append(&mut completed.request.replies);
                    continue;
                }
                let result = match completed.result {
                    Ok(refresh) => {
                        if refresh.watchable_roots != watched_roots {
                            watcher = None;
                            watcher_initializing = false;
                            watcher_retry = Instant::now();
                            pending_files.request(true);
                        }
                        apply_git_refresh(
                        &catalog,
                        &mut snapshot,
                        &mut subscribers,
                        &mut reviews,
                        protocol::ProjectCatalogRecord { generation: completed.generation, project: project.clone() },
                        refresh,
                        completed.request.full,
                    ).await
                    },
                    Err(error) => Err(error),
                };
                let explicit_refresh = !completed.request.replies.is_empty();
                for reply in completed.request.replies {
                    let _ = reply.send(result.clone());
                }
                if let Err(error) = result {
                    if explicit_refresh {
                        tracing::warn!(%project_id, %error, "explicit project Git refresh failed");
                        git_warning = Some(error);
                    } else {
                        warn_project_retry(&project_id, &mut subscribers, "project_git_status", error, &mut git_warning).await;
                    }
                } else if git_warning.take().is_some() {
                    tracing::info!(%project_id, "project Git refresh recovered");
                }
            }
        }
    }
    // Aborting these async wrappers would detach their running blocking reads.
    // Retirement stops publication and joins all owned reads before deletion.
    tracing::info!(target: "tyde::project_operation", stage = "cleanup",
        git_reads = git_tasks.len(), file_reads = file_tasks.len(),
        bootstrap_reads = initialization_tasks.len(), "settling owned project reads");
    drop(watcher);
    subscribers.clear();
    let (git, files, initialization, views) = tokio::join!(
        settle_project_reads(&mut git_tasks),
        settle_project_reads(&mut file_tasks),
        settle_project_reads(&mut initialization_tasks),
        settle_project_reads(&mut file_view_tasks),
    );
    git?;
    files?;
    initialization?;
    views?;
    settle_project_reads(&mut view_watch_tasks).await?;
    settle_project_reads(&mut observation_tasks).await?;
    tracing::info!(target: "tyde::project_operation", stage = "cleanup", "owned project reads settled");
    Ok(())
}

fn complete_after_file_watch(
    tasks: &mut JoinSet<()>,
    events: mpsc::UnboundedSender<notify::Result<Event>>,
    observation: notify::Result<Option<oneshot::Receiver<notify::Result<()>>>>,
    complete: impl FnOnce() + Send + 'static,
) {
    match observation {
        Ok(Some(response)) => {
            tasks.spawn(async move {
                let result = response
                    .await
                    .map_err(|_| notify::Error::generic("project filesystem watcher stopped"))
                    .and_then(|result| result);
                if let Err(error) = result {
                    let _ = events.send(Err(error));
                }
                complete();
            });
        }
        Ok(None) => complete(),
        Err(error) => {
            let _ = events.send(Err(error));
            complete();
        }
    }
}

async fn settle_project_reads<T: 'static>(tasks: &mut JoinSet<T>) -> Result<(), String> {
    let mut failed = false;
    while let Some(result) = tasks.join_next().await {
        failed |= result.is_err();
    }
    if failed {
        Err("project read worker stopped unexpectedly during cleanup".to_owned())
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct PendingFileRefresh {
    requested: bool,
    full: bool,
    replies: Vec<oneshot::Sender<Result<(), String>>>,
}

impl PendingFileRefresh {
    fn request(&mut self, full: bool) {
        self.requested = true;
        self.full |= full;
    }
}

struct CompletedFileRefresh {
    generation: u64,
    request: PendingFileRefresh,
    result: Result<FileScan, String>,
}

#[derive(Default)]
struct PendingGitRefresh {
    requested: bool,
    full: bool,
    refresh_diffs: bool,
    replies: Vec<oneshot::Sender<Result<(), String>>>,
}

impl PendingGitRefresh {
    fn request(&mut self, full: bool, refresh_diffs: bool) {
        self.requested = true;
        self.full |= full;
        self.refresh_diffs |= refresh_diffs;
    }
}

struct CompletedGitRefresh {
    generation: u64,
    project: Project,
    request: PendingGitRefresh,
    result: Result<GitRefresh, String>,
}

struct GitRefresh {
    watchable_roots: Vec<ProjectRootPath>,
    status: ProjectGitStatusPayload,
    serialized_status: Value,
    diffs: Vec<RefreshedProjectDiff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RememberedDiffContext {
    context_mode: DiffContextMode,
    generation: u64,
}

struct RefreshedProjectDiff {
    host_path: StreamPath,
    key: ProjectDiffRequestKey,
    context: RememberedDiffContext,
    result: Result<ProjectGitDiffPayload, String>,
}

fn read_git_refresh(
    project: &Project,
    previous_status: Option<Value>,
    remembered: HashMap<(StreamPath, ProjectDiffRequestKey), RememberedDiffContext>,
    refresh_diffs: bool,
) -> Result<GitRefresh, String> {
    let status = build_git_status(project)?;
    let serialized_status = serialize_git_status(&status)?;
    let status_changed = previous_status.as_ref() != Some(&serialized_status);
    tracing::debug!(project_id = %project.id, status_changed,
        remembered_diffs = remembered.len(), "refreshing project git state");
    let diffs = if refresh_diffs || status_changed {
        read_remembered_diffs(project, remembered)
    } else {
        Vec::new()
    };
    Ok(GitRefresh {
        watchable_roots: watchable_roots(project),
        status,
        serialized_status,
        diffs,
    })
}

async fn apply_git_refresh(
    catalog: &watch::Receiver<protocol::ProjectCatalogRecord>,
    snapshot: &mut ProjectSnapshotState,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    reviews: &mut ProjectReviews,
    record: protocol::ProjectCatalogRecord,
    refresh: GitRefresh,
    full: bool,
) -> Result<(), String> {
    let project = &record.project;
    let generation = record.generation;
    let status_changed = {
        let current = catalog.borrow();
        if current.generation != generation {
            return Ok(());
        }
        let changed = full || snapshot.git_status.as_ref() != Some(&refresh.serialized_status);
        if changed {
            snapshot.git_status = Some(refresh.serialized_status);
            fan_out_payload(subscribers, FrameKind::ProjectGitStatus, &refresh.status)?;
        }
        changed
    };
    if status_changed {
        reset_reviews_for_clean_unstaged_roots(reviews, project, &refresh.status).await;
    }
    for diff in refresh.diffs {
        let current = catalog.borrow();
        if current.generation != generation {
            return Ok(());
        }
        if snapshot
            .diff_context_modes
            .get(&(diff.host_path.clone(), diff.key))
            != Some(&diff.context)
        {
            continue;
        }
        let Some(subscriber) = subscribers.get(&diff.host_path) else {
            continue;
        };
        match diff.result {
            Ok(payload) => {
                let _ = send_payload(&subscriber.stream, FrameKind::ProjectGitDiff, &payload);
            }
            Err(error) => emit_project_command_error(
                &subscriber.stream,
                FrameKind::ProjectReadDiff,
                "project_read_diff",
                error,
                false,
            ),
        }
    }
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
    let mut disabled = 0;
    let mut ready = 0;
    let mut indexing = 0;
    let mut starting = 0;
    let mut unavailable = 0;
    let mut failed = 0;
    let mut error_count = 0;
    let mut warning_count = 0;
    for provider in roots.iter().flat_map(|root| root.providers.iter()) {
        match provider.state {
            CodeIntelState::Disabled => disabled += 1,
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
    let headline = if provider_count == 0 && disabled > 0 {
        CodeIntelOverviewHeadline::Disabled
    } else if provider_count == 0 {
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
        CodeIntelOverviewHeadline::Disabled => {
            Some("Code intelligence is off in Settings".to_owned())
        }
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
    raw_entries: &BTreeMap<ProjectRootPath, RootScan>,
) -> ProjectFileListPayload {
    let roots = project
        .root_paths()
        .into_iter()
        .map(|root| match raw_entries.get(&root) {
            Some(RootScan::Unavailable(message)) => ProjectRootListing {
                root,
                status: ProjectRootStatus::Unavailable {
                    message: message.clone(),
                },
                entries: Vec::new(),
                partial_dirs: Vec::new(),
            },
            Some(RootScan::Available(listing)) => ProjectRootListing {
                root,
                status: ProjectRootStatus::Available,
                entries: listing
                    .entries
                    .iter()
                    .map(|(path, kind)| ProjectFileEntry {
                        relative_path: path.clone(),
                        kind: *kind,
                        op: FileEntryOp::Add,
                    })
                    .collect(),
                partial_dirs: listing
                    .partial
                    .iter()
                    .map(|(relative_path, omitted)| ProjectPartialDir {
                        relative_path: relative_path.clone(),
                        omitted: *omitted,
                    })
                    .collect(),
            },
            None => ProjectRootListing {
                root,
                status: ProjectRootStatus::Available,
                entries: Vec::new(),
                partial_dirs: Vec::new(),
            },
        })
        .collect();
    ProjectFileListPayload {
        incremental: false,
        roots,
    }
}

async fn reset_reviews_for_clean_unstaged_roots(
    reviews: &mut ProjectReviews,
    project: &Project,
    git_status: &ProjectGitStatusPayload,
) {
    let roots = git_status
        .roots
        .iter()
        .filter(|root| root_unstaged_changes_are_clean(root))
        .map(|root| root.root.clone())
        .collect::<Vec<_>>();
    if !roots.is_empty() {
        reviews.reset_clean_roots(project, &roots).await;
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

fn emit_snapshot_to_stream(
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
    send_payload(stream, FrameKind::ProjectBootstrap, &bootstrap)?;
    if !file_delivery.wants_frame(FrameKind::CodeIntelOverview) {
        return Ok(());
    }
    send_payload(
        stream,
        FrameKind::CodeIntelOverview,
        &snapshot.code_intel_overview,
    )
}

fn fan_out_payload<T: serde::Serialize>(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("failed to serialize {kind} payload: {error}"))?;
    fan_out_value(subscribers, kind, payload);
    Ok(())
}

fn fan_out_value(
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

fn read_remembered_diffs(
    project: &Project,
    remembered: HashMap<(StreamPath, ProjectDiffRequestKey), RememberedDiffContext>,
) -> Vec<RefreshedProjectDiff> {
    let mut remembered = remembered.into_iter().collect::<Vec<_>>();
    remembered.sort_by(|((host_a, key_a), _), ((host_b, key_b), _)| {
        host_a
            .0
            .cmp(&host_b.0)
            .then_with(|| key_a.root.0.cmp(&key_b.root.0))
            .then_with(|| diff_scope_sort_key(key_a.scope).cmp(&diff_scope_sort_key(key_b.scope)))
            .then_with(|| key_a.path.cmp(&key_b.path))
    });
    remembered
        .into_iter()
        .map(|((host_path, key), context)| {
            let payload = ProjectReadDiffPayload {
                request_id: None,
                root: key.root.clone(),
                scope: key.scope,
                revision: key.revision.clone(),
                path: key.path.clone(),
                context_mode: context.context_mode,
                out_of_band: false,
            };
            RefreshedProjectDiff {
                host_path,
                key,
                context,
                result: read_diff(project, payload),
            }
        })
        .collect()
}

fn diff_scope_sort_key(scope: ProjectDiffScope) -> u8 {
    match scope {
        ProjectDiffScope::Staged => 0,
        ProjectDiffScope::Unstaged => 1,
        ProjectDiffScope::Uncommitted => 2,
    }
}

fn send_payload<T: serde::Serialize>(
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

async fn warn_project_retry(
    project_id: &ProjectId,
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    operation: &str,
    error: String,
    warning: &mut Option<String>,
) {
    let message = format!(
        "{error} The project remains available; automatic recovery will retry in a few seconds."
    );
    if warning.as_ref() != Some(&message) {
        tracing::warn!(%project_id, operation, %error, retry_ms = PROJECT_RETRY_INTERVAL.as_millis(), "project background operation failed; retaining subscription");
    }
    if warning.is_none() {
        emit_project_stream_warning(subscribers, operation, message.clone()).await;
    }
    *warning = Some(message);
}

fn project_watch_limit_guidance(error: &str) -> String {
    format!(
        "{error} Live project file updates are disabled, but the project remains available. For best results, configure each project root as the root of its Git repository instead of a broader parent directory. On Linux, you can also increase fs.inotify.max_user_watches. Watching will resume automatically after correcting the root or system limit."
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
        );
    }
}

fn emit_project_command_error(
    stream: &Stream,
    request_kind: FrameKind,
    operation: &str,
    message: String,
    fatal: bool,
) {
    let payload = CommandErrorPayload {
        workbench_reason: None,
        context: None,
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

/// Most entries in one root's listing. Bootstraps reach every desktop client
/// whenever a project or workbench appears, so an unbounded tree blocks the
/// UI while it is decoded and rendered, and can exceed the frame size limit.
const FILE_LIST_ENTRY_BUDGET: usize = 256;

/// Small bootstrap preview only. File access uses retained, paged views rather
/// than growing this shared snapshot with every client's expanded folders.
fn scan_project_files(project: &Project) -> Result<FileScan, String> {
    let mut roots = BTreeMap::new();
    for root in project.root_paths() {
        let scan = match root_unavailable_reason(&root) {
            Some(message) => RootScan::Unavailable(message),
            None => {
                let root_path = Path::new(&root.0);
                match collect_raw_entries(root_path, root_path, DEFAULT_FILE_LIST_DEPTH) {
                    Ok(listing) => {
                        let mut entries = RootEntries::default();
                        entries.add("", listing);
                        RootScan::Available(entries)
                    }
                    // The root itself disappeared while it was being scanned.
                    Err(error) => match root_unavailable_reason(&root) {
                        Some(message) => RootScan::Unavailable(message),
                        None => return Err(error),
                    },
                }
            }
        };
        if let RootScan::Unavailable(message) = &scan {
            tracing::debug!(%root, %message, "project root is unavailable");
        }
        roots.insert(root, scan);
    }
    Ok(FileScan { roots })
}

fn send_file_view_state(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    requester: &StreamPath,
    state: ProjectFileViewStatePayload,
) {
    if let Some(subscriber) = subscribers.get(requester)
        && let Err(error) =
            send_payload(&subscriber.stream, FrameKind::ProjectFileViewState, &state)
    {
        tracing::warn!(%error, "file view subscriber closed");
        subscribers.remove(requester);
    }
}

fn file_view_key(view: &ProjectFileView) -> Option<ProjectPath> {
    match view {
        ProjectFileView::Directory { path, .. } => Some(path.clone()),
        ProjectFileView::QuickOpen { .. } => None,
    }
}

fn retained_directory_children(
    subscriber: &ProjectSubscriber,
    parent: &Option<ProjectPath>,
) -> HashSet<ProjectPath> {
    let Some(parent) = parent else {
        return HashSet::new();
    };
    subscriber
        .views
        .keys()
        .flatten()
        .filter(|&path| {
            path.root == parent.root
                && path != parent
                && !path.relative_path.is_empty()
                && Path::new(&path.relative_path).parent() == Some(Path::new(&parent.relative_path))
        })
        .cloned()
        .collect()
}

fn invalidate_file_views(
    subscribers: &mut HashMap<StreamPath, ProjectSubscriber>,
    event: Option<&Event>,
    roots: &WatcherRootPaths,
) -> bool {
    if event.is_some_and(|event| {
        matches!(
            event.kind,
            EventKind::Access(_) | EventKind::Modify(notify::event::ModifyKind::Data(_))
        )
    }) {
        return false;
    }
    let mut pending = false;
    for subscriber in subscribers.values_mut() {
        for read in subscriber.views.values_mut() {
            let affected = match (&read.view, event) {
                (_, None) => true,
                (ProjectFileView::QuickOpen { .. }, Some(event)) => {
                    event.paths.is_empty() || event.paths.iter().any(|path| !is_inside_git(path))
                }
                (ProjectFileView::Directory { path, filter, .. }, Some(event)) => roots
                    .prefixes
                    .iter()
                    .filter(|(root, _)| *root == path.root)
                    .any(|(_, prefixes)| {
                        prefixes.iter().any(|prefix| {
                            let dir = prefix.join(&path.relative_path);
                            event.paths.is_empty()
                                || event.paths.iter().any(|changed| {
                                    changed.parent() == Some(dir.as_path())
                                        || dir.starts_with(changed)
                                        || (!filter.is_empty()
                                            && changed.starts_with(&dir)
                                            && !is_inside_git(changed))
                                })
                        })
                    }),
            };
            pending |= affected;
            if event.is_some() {
                read.refresh_pending |= affected;
            } else {
                read.dirty |= affected;
            }
        }
    }
    pending
}

type FileOrder = (std::cmp::Reverse<u32>, bool, String, String, String);

fn file_order(entry: &ProjectFileCursor) -> FileOrder {
    (
        std::cmp::Reverse(entry.score),
        entry.kind != ProjectFileKind::Directory,
        entry.path.root.0.clone(),
        entry.path.relative_path.to_lowercase(),
        entry.path.relative_path.clone(),
    )
}

const FILE_VIEW_PAGE_SIZE: usize = 128;

/// Selection is bounded even for a flat directory with millions of children.
/// Cursors are value boundaries, not offsets into an evictable server cache.
fn validate_directory_view(project: &Project, path: &ProjectPath) -> Result<(), String> {
    validate_root(project, &path.root)?;
    if !path.relative_path.is_empty() {
        validate_relative_path(&path.relative_path)?;
    }
    Ok(())
}

fn read_file_view(
    project: &Project,
    view: ProjectFileView,
    retained: &HashSet<ProjectPath>,
    cancel: &AtomicBool,
) -> Result<ProjectFileViewState, String> {
    let window = match &view {
        ProjectFileView::Directory { window, .. } | ProjectFileView::QuickOpen { window, .. } => {
            window
        }
    };
    let boundary = match window {
        ProjectFileWindow::First => None,
        ProjectFileWindow::After(cursor) | ProjectFileWindow::Before(cursor) => {
            Some(file_order(cursor))
        }
    };
    let backwards = matches!(window, ProjectFileWindow::Before(_));
    let mut selected = BTreeMap::new();
    let mut retained_entries = BTreeMap::new();
    let mut has_before = false;
    let mut has_after = false;
    let mut admit = |entry: ProjectFileCursor| {
        let key = file_order(&entry);
        if let Some(boundary) = &boundary {
            if backwards && key >= *boundary {
                has_after = true;
                return;
            }
            if !backwards && key <= *boundary {
                has_before = true;
                return;
            }
        }
        selected.insert(key, entry);
        if selected.len() > FILE_VIEW_PAGE_SIZE {
            if backwards {
                selected.pop_first();
                has_before = true;
            } else {
                selected.pop_last();
                has_after = true;
            }
        }
    };
    match &view {
        ProjectFileView::Directory {
            path,
            show_hidden,
            filter,
            ..
        } => {
            validate_directory_view(project, path)?;
            let dir = Path::new(&path.root.0).join(&path.relative_path);
            #[cfg(feature = "test-support")]
            scan_test_support::run(&dir, scan_test_support::ScanPoint::ReadDirectory);
            let children = match fs::read_dir(&dir) {
                Ok(children) => children,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ProjectFileViewState::DirectoryMissing);
                }
                Err(error) => return Err(format!("Read directory '{}': {error}", dir.display())),
            };
            let filter = filter.to_lowercase();
            for child in children {
                if cancel.load(Ordering::Relaxed) {
                    return Err("File view cancelled".to_owned());
                }
                let child = child
                    .map_err(|error| format!("Enumerate directory '{}': {error}", dir.display()))?;
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| "File name is not valid UTF-8".to_owned())?;
                if name == ".git" || (!show_hidden && name.starts_with('.')) {
                    continue;
                }
                let kind = child
                    .file_type()
                    .map_err(|error| format!("Read file type: {error}"))?;
                if !filter.is_empty()
                    && !name.to_lowercase().contains(&filter)
                    && !(kind.is_dir()
                        && directory_matches_filter(&child.path(), &filter, *show_hidden, cancel)?)
                {
                    continue;
                }
                let relative_path = if path.relative_path.is_empty() {
                    name
                } else {
                    format!("{}/{name}", path.relative_path)
                };
                let entry = ProjectFileCursor {
                    path: ProjectPath {
                        root: path.root.clone(),
                        relative_path,
                    },
                    kind: file_kind(kind),
                    score: 0,
                };
                if entry.kind == ProjectFileKind::Directory && retained.contains(&entry.path) {
                    retained_entries.insert(file_order(&entry), entry.clone());
                }
                admit(entry);
            }
        }
        ProjectFileView::QuickOpen { query, .. } => {
            if query.len() > 4096 {
                return Err("File query exceeds 4096 bytes".to_owned());
            }
            for root in project.root_paths() {
                validate_root(project, &root)?;
                let walk = ignore::WalkBuilder::new(&root.0)
                    .standard_filters(false)
                    .follow_links(false)
                    .filter_entry(|entry| entry.file_name() != ".git")
                    .build();
                for entry in walk {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("File view cancelled".to_owned());
                    }
                    let entry = entry.map_err(|error| format!("Find project files: {error}"))?;
                    let Some(kind) = entry.file_type() else {
                        return Err("Missing file type while finding project files".to_owned());
                    };
                    if kind.is_dir() {
                        continue;
                    }
                    let relative = entry
                        .path()
                        .strip_prefix(&root.0)
                        .map_err(|error| format!("File outside project root: {error}"))?;
                    let relative_path = relative
                        .to_str()
                        .ok_or("File path is not valid UTF-8")?
                        .replace('\\', "/");
                    if let Some(score) = file_name_score(query.trim(), &relative_path) {
                        admit(ProjectFileCursor {
                            path: ProjectPath {
                                root: root.clone(),
                                relative_path,
                            },
                            kind: file_kind(kind),
                            score,
                        });
                    }
                }
            }
        }
    }
    let entries: Vec<_> = selected.values().cloned().collect();
    let cursor = match window {
        ProjectFileWindow::First => None,
        ProjectFileWindow::After(cursor) | ProjectFileWindow::Before(cursor) => Some(cursor),
    };
    let previous = has_before
        .then(|| entries.first().or(cursor).cloned())
        .flatten();
    let next = has_after
        .then(|| entries.last().or(cursor).cloned())
        .flatten();
    // Active child declarations pin real directory rows, never historical entries.
    selected.extend(retained_entries);
    let state = ProjectFileViewState::Ready {
        entries: selected.into_values().collect(),
        previous,
        next,
    };
    ensure_json_payload_is_transportable(&state, "file view")?;
    Ok(state)
}

fn directory_matches_filter(
    directory: &Path,
    filter: &str,
    show_hidden: bool,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let walk = ignore::WalkBuilder::new(directory)
        .standard_filters(false)
        .hidden(!show_hidden)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk {
        if cancel.load(Ordering::Relaxed) {
            return Err("File view cancelled".to_owned());
        }
        let entry = entry.map_err(|error| format!("Filter directory: {error}"))?;
        if entry
            .file_name()
            .to_str()
            .ok_or("File name is not valid UTF-8")?
            .to_lowercase()
            .contains(filter)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_kind(kind: fs::FileType) -> ProjectFileKind {
    if kind.is_dir() {
        ProjectFileKind::Directory
    } else if kind.is_symlink() {
        ProjectFileKind::Symlink
    } else {
        ProjectFileKind::File
    }
}

fn file_name_score(query: &str, path: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let path = path.to_lowercase();
    let name = path.rsplit('/').next()?;
    if name == query {
        return Some(1_000_000);
    }
    if name.starts_with(&query) {
        return Some(500_000);
    }
    if name.contains(&query) {
        return Some(250_000);
    }
    let mut wanted = query.chars();
    let mut next = wanted.next();
    let mut gaps = 0_u32;
    for ch in path.chars() {
        if next == Some(ch) {
            next = wanted.next();
            if next.is_none() {
                return Some(100_000_u32.saturating_sub(gaps));
            }
        } else {
            gaps = gaps.saturating_add(1);
        }
    }
    None
}

fn root_unavailable_reason(root: &ProjectRootPath) -> Option<String> {
    match fs::metadata(&root.0) {
        Ok(metadata) if metadata.is_dir() => None,
        Ok(_) => Some(format!("Project root '{root}' is not a directory")),
        Err(err) => Some(format!("Failed to stat project root '{root}': {err}")),
    }
}

pub(crate) fn build_git_status(project: &Project) -> Result<ProjectGitStatusPayload, String> {
    let started = std::time::Instant::now();
    let result = build_git_status_with_runner(project, run_git_mode);
    tracing::debug!(project_id = %project.id, elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(), "completed project git status read");
    result
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
        // Reported by the file listing; a status entry would misreport it clean.
        if root_unavailable_reason(&root).is_some() {
            continue;
        }
        let output = match run_git(
            &root.0,
            &["status", "--porcelain=v2", "--branch", "-z"],
            GitAccessMode::ReadOnly,
        ) {
            Ok(output) => output,
            // Match the file scanner's disappearance race: availability is
            // reported by the root listing, not as a failed healthy project.
            Err(_) if root_unavailable_reason(&root).is_some() => {
                tracing::debug!(
                    "project root disappeared during Git status; root listing owns availability"
                );
                continue;
            }
            Err(err) if is_not_git_repository_error(&err) => {
                if root_is_git_repository(&root.0) {
                    tracing::warn!(
                        error_len = err.len(),
                        "Git could not read an existing repository; preserving project state"
                    );
                    return Err(err);
                }
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

        let mut records = output.split_terminator('\0');
        while let Some(line) = records.next() {
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
                let renamed_or_copied = line.starts_with("2 ");
                let field_count = if renamed_or_copied { 10 } else { 9 };
                let parts: Vec<&str> = line.splitn(field_count, ' ').collect();
                if parts.len() != field_count || parts[field_count - 1].is_empty() {
                    return Err("invalid Git status path record".to_owned());
                }
                let xy = parts[1];
                if xy.len() != 2 || !xy.is_ascii() {
                    return Err("invalid Git XY status".to_owned());
                }
                let path = parts[field_count - 1].to_owned();
                if renamed_or_copied && records.next().is_none_or(str::is_empty) {
                    return Err("missing Git rename/copy source path".to_owned());
                }
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
    let Some(absolute) = resolve_project_file_path(project, &path)? else {
        return Ok(missing_file_payload(path));
    };
    // `version` is a placeholder here; the project-stream actor overwrites it
    // with the centralized counter's next value (the single bump point) before
    // the payload leaves the actor. See `ProjectStreamCommand::ReadFile`.
    let mut file = match File::open(&absolute) {
        Ok(file) => file,
        // Deletion is an answer, not a failure: report it as a typed payload
        // (a command error carries no path, so the client could not attribute
        // it to the right viewer).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(missing_file_payload(path));
        }
        Err(err) => {
            return Ok(unreadable_file_payload(path, &absolute, err));
        }
    };
    let initial_size_bytes = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(
        initial_size_bytes
            .min(FILE_READ_LIMIT_BYTES)
            .try_into()
            .unwrap_or(TEXT_READ_LIMIT_BYTES as usize),
    );
    if let Err(err) = (&mut file)
        .take(FILE_READ_LIMIT_BYTES)
        .read_to_end(&mut bytes)
    {
        return Ok(unreadable_file_payload(path, &absolute, err));
    }
    let final_size_bytes = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(initial_size_bytes);
    let observed_size_bytes = initial_size_bytes
        .max(final_size_bytes)
        .max(bytes.len() as u64);
    let sniffed_mime = sniff_binary_mime(&bytes);
    if sniffed_mime.is_some() || bytes.contains(&0) {
        let mime_type = sniffed_mime
            .or_else(|| binary_mime_from_path(&absolute))
            .unwrap_or("application/octet-stream");
        return Ok(binary_file_payload(
            path,
            bytes,
            mime_type,
            observed_size_bytes,
        ));
    }
    match String::from_utf8(bytes) {
        Ok(mut contents) => {
            let truncated = contents.len() as u64 > TEXT_READ_LIMIT_BYTES
                || observed_size_bytes > TEXT_READ_LIMIT_BYTES;
            if contents.len() as u64 > TEXT_READ_LIMIT_BYTES {
                let mut end = TEXT_READ_LIMIT_BYTES as usize;
                while !contents.is_char_boundary(end) {
                    end -= 1;
                }
                contents.truncate(end);
            }
            Ok(text_file_payload(
                path,
                contents,
                observed_size_bytes,
                truncated,
            ))
        }
        Err(error)
            if observed_size_bytes > TEXT_READ_LIMIT_BYTES
                && error.utf8_error().error_len().is_none()
                && error.utf8_error().valid_up_to() + 4 >= TEXT_READ_LIMIT_BYTES as usize =>
        {
            let valid_up_to = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            let contents = String::from_utf8(bytes)
                .map_err(|_| "validated UTF-8 prefix could not be decoded".to_owned())?;
            Ok(text_file_payload(path, contents, observed_size_bytes, true))
        }
        Err(error) => Ok(binary_file_payload(
            path,
            error.into_bytes(),
            binary_mime_from_path(&absolute).unwrap_or("application/octet-stream"),
            observed_size_bytes,
        )),
    }
}

pub(crate) async fn open_project_path(
    project: &Project,
    payload: ProjectOpenPathPayload,
    opener_program: Option<&Path>,
) -> Result<(), String> {
    let canonical_path = resolve_project_file_path(project, &payload.path)?.ok_or_else(|| {
        format!(
            "Project path '{}' does not exist",
            payload.path.relative_path
        )
    })?;
    if !canonical_path.is_file() {
        return Err(format!(
            "Project path '{}' is not a regular file",
            payload.path.relative_path
        ));
    }

    let command = project_path_command(&canonical_path, payload.action, opener_program)?;
    command
        .launch_independent(crate::process_env::launch::IndependentProcess::ExternalApplication)
        .await
        .map_err(|error| {
            format!(
                "Failed to {} '{}': {error}",
                match payload.action {
                    ProjectOpenPathAction::OpenExternally => "open",
                    ProjectOpenPathAction::Reveal => "reveal",
                },
                payload.path.relative_path
            )
        })?;
    Ok(())
}

pub(crate) fn resolve_project_file_path(
    project: &Project,
    path: &ProjectPath,
) -> Result<Option<PathBuf>, String> {
    validate_project_path(project, path)?;
    let canonical_root = match fs::canonicalize(&path.root.0) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to resolve project root '{}': {error}",
                path.root
            ));
        }
    };
    let unresolved = absolute_project_path(path)?;
    let canonical_path = match fs::canonicalize(&unresolved) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to resolve file '{}': {error}",
                unresolved.display()
            ));
        }
    };
    if !canonical_path.starts_with(&canonical_root) {
        return Err(format!(
            "Refusing to read or open '{}' because it resolves outside project root '{}'",
            path.relative_path, path.root
        ));
    }
    Ok(Some(canonical_path))
}

fn project_path_command(
    path: &Path,
    action: ProjectOpenPathAction,
    opener_program: Option<&Path>,
) -> Result<Command, String> {
    if let Some(program) = opener_program {
        let mut command = crate::process_env::command(program)?;
        command
            .arg(match action {
                ProjectOpenPathAction::OpenExternally => "open",
                ProjectOpenPathAction::Reveal => "reveal",
            })
            .arg(path);
        return Ok(command);
    }

    #[cfg(target_os = "macos")]
    {
        let mut command = crate::process_env::command("open")?;
        if matches!(action, ProjectOpenPathAction::Reveal) {
            command.arg("-R");
        }
        command.arg(path);
        Ok(command)
    }
    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsString;
        match action {
            ProjectOpenPathAction::OpenExternally => {
                let mut command = crate::process_env::command("rundll32.exe")?;
                command.arg("url.dll,FileProtocolHandler").arg(path);
                Ok(command)
            }
            ProjectOpenPathAction::Reveal => {
                let mut argument = OsString::from("/select,");
                argument.push(path);
                let mut command = crate::process_env::command("explorer.exe")?;
                command.arg(argument);
                Ok(command)
            }
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let mut command = crate::process_env::command("xdg-open")?;
        command.arg(match action {
            ProjectOpenPathAction::OpenExternally => path,
            ProjectOpenPathAction::Reveal => path.parent().ok_or_else(|| {
                format!("Cannot reveal file without a parent: {}", path.display())
            })?,
        });
        Ok(command)
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

fn missing_file_payload(path: ProjectPath) -> ProjectFileContentsPayload {
    ProjectFileContentsPayload {
        path,
        version: ProjectFileVersion(0),
        contents: None,
        is_binary: false,
        missing: true,
        binary: None,
    }
}

fn text_file_payload(
    path: ProjectPath,
    mut contents: String,
    size_bytes: u64,
    truncated: bool,
) -> ProjectFileContentsPayload {
    if truncated {
        let displayed_bytes = contents.len();
        contents.push_str(&format!(
            "\n\n[Tyde displayed a bounded {displayed_bytes} byte prefix of this {size_bytes} byte text file.]"
        ));
    }
    let mut payload = ProjectFileContentsPayload {
        path,
        version: ProjectFileVersion(0),
        contents: Some(contents),
        is_binary: false,
        missing: false,
        binary: None,
    };
    let encoded_contents_bound = payload
        .contents
        .as_deref()
        .and_then(|contents| {
            contents.bytes().try_fold(2_usize, |size, byte| {
                size.checked_add(match byte {
                    b'"' | b'\\' => 2,
                    0x00..=0x1f => 6,
                    _ => 1,
                })
            })
        })
        .unwrap_or(usize::MAX);
    let fits = encoded_contents_bound
        .checked_add(TEXT_PROTOCOL_RESERVE_BYTES)
        .is_some_and(|encoded| encoded < protocol::framing::MAX_LOGICAL_HEADER);
    if !fits {
        payload.contents = Some(format!(
            "Unable to display this text file because its escaped contents exceed Tyde's {} MiB protocol limit.",
            protocol::framing::MAX_LOGICAL_HEADER / 1024 / 1024
        ));
    }
    payload
}

fn binary_file_payload(
    path: ProjectPath,
    bytes: Vec<u8>,
    mime_type: &str,
    size_bytes: u64,
) -> ProjectFileContentsPayload {
    let supported = is_supported_preview_mime(mime_type);
    let within_preview_limit = size_bytes <= BINARY_PREVIEW_LIMIT_BYTES
        && bytes.len() as u64 <= BINARY_PREVIEW_LIMIT_BYTES;
    ProjectFileContentsPayload {
        path,
        version: ProjectFileVersion(0),
        contents: None,
        is_binary: true,
        missing: false,
        binary: Some(ProjectBinaryFilePayload {
            mime_type: mime_type.to_owned(),
            size_bytes,
            data_base64: (supported && within_preview_limit).then(|| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(bytes)
            }),
            preview_error: if !within_preview_limit {
                Some(format!(
                    "Preview unavailable because this file exceeds the {} MiB limit.",
                    BINARY_PREVIEW_LIMIT_BYTES / 1024 / 1024
                ))
            } else {
                (!supported)
                    .then(|| "Tyde does not have an inline preview for this file type.".to_owned())
            },
        }),
    }
}

pub(crate) fn sniff_binary_mime(bytes: &[u8]) -> Option<&'static str> {
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
        sniff_iso_bmff_mime(bytes)
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

fn sniff_iso_bmff_mime(bytes: &[u8]) -> Option<&'static str> {
    let declared_size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    let (major_start, compatible_start, minimum_size) = if declared_size == 1 {
        (16, 24, 24)
    } else {
        (8, 16, 16)
    };
    let box_size = match declared_size {
        0 => bytes.len(),
        1 => usize::try_from(u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?)).ok()?,
        size => size as usize,
    };
    if box_size < minimum_size || box_size > bytes.len() {
        return None;
    }

    let major: &[u8; 4] = bytes.get(major_start..major_start + 4)?.try_into().ok()?;
    let (compatible_brands, _) = bytes[compatible_start..box_size].as_chunks::<4>();
    let mut avif = false;
    let mut heic = false;
    let mut heif = false;
    let mut audio = false;
    let mut video = false;
    for brand in std::iter::once(major).chain(compatible_brands) {
        match brand {
            b"avif" | b"avis" => avif = true,
            b"heic" | b"heix" | b"hevc" | b"hevx" => heic = true,
            b"mif1" | b"msf1" => heif = true,
            b"M4A " | b"M4B " | b"M4P " => audio = true,
            b"isom" | b"iso2" | b"iso3" | b"iso4" | b"iso5" | b"iso6" | b"mp41" | b"mp42"
            | b"avc1" | b"M4V " | b"qt  " => video = true,
            _ => {}
        }
    }
    if avif {
        Some("image/avif")
    } else if heic {
        Some("image/heic")
    } else if heif {
        Some("image/heif")
    } else if audio {
        Some("audio/mp4")
    } else if video {
        Some("video/mp4")
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
    let diff = build_diff(project, payload)?;
    ensure_json_payload_is_transportable(&diff, "diff")?;
    Ok(diff)
}

// Internal review state is not a wire response. Keep the same Git/parser path,
// but enforce transport bounds only when sending a genuine client payload.
pub(crate) fn build_diff(
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
        "--numstat",
        "-z",
        "--patch",
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

    let diff = ProjectGitDiffPayload {
        request_id: payload.request_id,
        root: payload.root,
        scope: payload.scope,
        revision: payload.revision,
        path: payload.path,
        context_mode: payload.context_mode,
        files,
    };
    Ok(diff)
}

/// Counts what serializing would write without keeping any of it, so an
/// oversized diff is rejected without first building a second copy of it in
/// memory.
struct SerializedLen(usize);

impl std::io::Write for SerializedLen {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn ensure_json_payload_is_transportable(
    payload: &impl serde::Serialize,
    description: &str,
) -> Result<(), String> {
    let mut length = SerializedLen(0);
    serde_json::to_writer(&mut length, payload)
        .map_err(|error| format!("failed to measure {description} response: {error}"))?;
    if length.0 > DIFF_PAYLOAD_LIMIT_BYTES {
        return Err(format!(
            "{description} is too large to send: {} bytes exceeds the {DIFF_PAYLOAD_LIMIT_BYTES} byte \
             limit for one response",
            length.0
        ));
    }
    Ok(())
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

    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
    if let Some(path) = path {
        args.push("--");
        args.push(path);
    }

    let raw = run_git(root, &args, GitAccessMode::ReadOnly)?;
    Ok(raw.split_terminator('\0').map(ToOwned::to_owned).collect())
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
        &[
            "diff",
            "--numstat",
            "-z",
            "--patch",
            "--",
            &path.relative_path,
        ],
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

#[cfg(feature = "test-support")]
pub mod scan_test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum ScanPoint {
        ActorStarting,
        Metadata,
        ReadDirectory,
        FileRefreshSettled,
        WatcherInitialize,
        WatcherInitialized,
        WatcherReady,
        WatcherAdopted,
        WatcherQueued,
        WatcherProcess,
        WatcherReconcile,
        WatcherProcessed,
        WatcherRenameProcessed,
        WatcherOverflow,
        WatcherDropped,
        WatcherFailure,
        WatcherUnwatchFailed,
        WatcherDispatch,
        WatcherKernelRescan,
        WatcherRescanFinished,
        WatcherRescanStarting,
        WatcherForceRescan,
        WatcherRescanDelivered,
        WatcherViewQueued,
        WatcherViewApplying,
        WatcherInvalidated,
        WatcherRegisterPending,
        WatcherRegistered,
        WatcherUnregistered,
        FileObservationWaiting,
        FileViewRouting,
        FileViewBackpressure,
        DevFrontendProbe,
        PreferencesPinsApplied,
    }

    type ScanKey = (PathBuf, ScanPoint);
    type ScanAction = Box<dyn FnOnce() + Send>;
    static HOOKS: OnceLock<Mutex<HashMap<ScanKey, ScanAction>>> = OnceLock::new();

    pub struct InstalledScanHook {
        key: ScanKey,
    }

    impl InstalledScanHook {
        pub fn install(path: PathBuf, point: ScanPoint, action: ScanAction) -> Self {
            let key = (path, point);
            let mut hooks = HOOKS.get_or_init(Mutex::default).lock().unwrap();
            assert!(!hooks.contains_key(&key), "scan hook already installed");
            hooks.insert(key.clone(), action);
            Self { key }
        }
    }

    impl Drop for InstalledScanHook {
        fn drop(&mut self) {
            HOOKS
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .remove(&self.key);
        }
    }

    pub(crate) fn take(path: &Path, point: ScanPoint) -> Option<ScanAction> {
        HOOKS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .remove(&(path.to_owned(), point))
    }

    pub(crate) fn run(path: &Path, point: ScanPoint) -> bool {
        if let Some(action) = take(path, point) {
            action();
            true
        } else {
            false
        }
    }
}

/// Entries below one listing start; `omitted` counts the start's children
/// left out at the budget.
struct Listing {
    entries: Vec<RawFileEntry>,
    omitted: usize,
}

/// Lists entries below `start` breadth-first, `max_depth + 1` levels deep and
/// at most `FILE_LIST_ENTRY_BUDGET` entries. `start` keeps its children in path
/// order up to the budget and counts the rest; a deeper directory lists its
/// children only when all of them fit, so no other directory is partial.
/// Directories left unlisted load on demand through `ProjectFileView`. A
/// directory that vanishes before it is read is omitted.
fn collect_raw_entries(root: &Path, start: &Path, max_depth: usize) -> Result<Listing, String> {
    let mut out: Vec<Option<RawFileEntry>> = Vec::new();
    let mut omitted = 0;
    let mut queue = VecDeque::from([(start.to_path_buf(), None::<usize>, 0usize)]);
    while let Some((current, entry_index, depth)) = queue.pop_front() {
        let remaining = FILE_LIST_ENTRY_BUDGET - out.len();
        let children = match read_listing_children(&current, entry_index.is_none(), remaining)? {
            ListingChildren::Listed { children, cut } => {
                omitted += cut;
                children
            }
            ListingChildren::Vanished => {
                if let Some(index) = entry_index {
                    out[index] = None;
                }
                continue;
            }
            ListingChildren::OverBudget => continue,
        };

        for path in children {
            #[cfg(feature = "test-support")]
            scan_test_support::run(&path, scan_test_support::ScanPoint::Metadata);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "project scan metadata failed after directory enumeration");
                    if error.kind() == std::io::ErrorKind::NotFound {
                        continue;
                    }
                    return Err(format!("Failed to stat path '{}': {error}", path.display()));
                }
            };
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

            out.push(Some((relative_path, kind)));
            if metadata.is_dir() && depth < max_depth {
                queue.push_back((path, Some(out.len() - 1), depth + 1));
            }
        }
        if out.len() == FILE_LIST_ENTRY_BUDGET {
            break;
        }
    }

    Ok(Listing {
        entries: out.into_iter().flatten().collect(),
        omitted,
    })
}

enum ListingChildren {
    Listed { children: Vec<PathBuf>, cut: usize },
    Vanished,
    OverBudget,
}

/// Reads one directory's children, excluding `.git`. Only the listing start
/// is truncated; any other directory with more children than `remaining` is
/// abandoned without reading the rest of it.
fn read_listing_children(
    current: &Path,
    is_start: bool,
    remaining: usize,
) -> Result<ListingChildren, String> {
    #[cfg(feature = "test-support")]
    scan_test_support::run(current, scan_test_support::ScanPoint::ReadDirectory);
    let directory = match fs::read_dir(current) {
        Ok(directory) => directory,
        Err(error) if !is_start && error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %current.display(), %error, "project scan directory disappeared before descent");
            return Ok(ListingChildren::Vanished);
        }
        Err(error) => {
            return Err(format!(
                "Failed to read directory '{}': {error}",
                current.display()
            ));
        }
    };
    let mut children = std::collections::BinaryHeap::new();
    let mut count: usize = 0;
    for entry in directory {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %current.display(), %error, "project scan entry disappeared during enumeration");
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "Failed to iterate directory '{}': {error}",
                    current.display()
                ));
            }
        };
        if entry.file_name() == ".git" {
            continue;
        }
        count += 1;
        children.push(entry.path());
        if !is_start && children.len() > remaining {
            return Ok(ListingChildren::OverBudget);
        }
        if children.len() > remaining {
            children.pop();
        }
    }
    let children = children.into_sorted_vec();
    let cut = count.saturating_sub(remaining);
    Ok(ListingChildren::Listed { children, cut })
}

fn validate_root(project: &Project, root: &ProjectRootPath) -> Result<(), String> {
    if project.is_removing_workbench() {
        return Err("Workbench removal is in progress".to_owned());
    }
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
    let host_path = project.host_path_for_runtime(absolute_path).ok()?;
    let absolute = Path::new(&host_path);
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

pub(crate) fn run_git_mode(
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Result<String, String> {
    run_git_mode_with_binary("git", root, args, access_mode)
}

fn run_git_lossy_mode(
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Result<String, String> {
    let output = spawn_git_command("git", root, args, access_mode, false)?
        .wait_with_output()
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
    let output = spawn_git_command(git_binary, root, args, access_mode, false)?
        .wait_with_output()
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
    let mut child = spawn_git_command(git_binary, root, args, access_mode, true)?;

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

fn spawn_git_command(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
    pipe_stdin: bool,
) -> Result<crate::process_env::launch::BlockingChild, String> {
    git_command(git_binary, root, args, access_mode)?
        .stdin(if pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_blocking()
        .map_err(|error| format!("Failed to start git in '{root}': {error}"))
}

fn git_command(
    git_binary: impl AsRef<std::ffi::OsStr>,
    root: &str,
    args: &[&str],
    access_mode: GitAccessMode,
) -> Result<Command, String> {
    let mut command = crate::process_env::command(git_binary)?;
    command.env("LC_ALL", "C");
    if matches!(access_mode, GitAccessMode::ReadOnly) {
        command.arg("--no-optional-locks");
    }
    command
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(args);
    Ok(command)
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
    // Git emits combined patches before ordinary numstat records. Ordinary
    // paths come from -z metadata, never the ambiguous presentation headers.
    let mut metadata_start = 0;
    for line in raw.split_inclusive('\n') {
        let mut fields = line.splitn(3, '\t');
        if let (Some(added), Some(deleted), Some(_)) = (fields.next(), fields.next(), fields.next())
            && ((added == "-" && deleted == "-")
                || [added, deleted].into_iter().all(|field| {
                    !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit())
                }))
        {
            break;
        }
        metadata_start += line.len();
    }
    let (combined, mut remaining) = raw.split_at(metadata_start);
    let mut paths = VecDeque::new();
    while !remaining.is_empty() && !remaining.starts_with('\0') {
        let (_, path) = remaining
            .split_once('\t')
            .and_then(|(_, remaining)| remaining.split_once('\t'))
            .ok_or_else(|| "invalid Git numstat record".to_owned())?;
        let (mut path, rest) = path
            .split_once('\0')
            .ok_or_else(|| "unterminated Git numstat path".to_owned())?;
        remaining = rest;
        if path.is_empty() {
            let (source, rest) = remaining
                .split_once('\0')
                .ok_or_else(|| "missing Git diff rename/copy source".to_owned())?;
            if source.is_empty() {
                return Err("empty Git diff rename/copy source".to_owned());
            }
            (path, remaining) = rest
                .split_once('\0')
                .ok_or_else(|| "missing Git diff rename/copy destination".to_owned())?;
        }
        if path.is_empty() {
            return Err("empty Git diff path".to_owned());
        }
        paths.push_back(path);
    }
    let patches = if paths.is_empty() {
        remaining
    } else {
        remaining
            .strip_prefix('\0')
            .ok_or_else(|| "missing Git patch separator".to_owned())?
    };
    let mut files = Vec::new();
    let mut current_file: Option<ParsedGitDiffFile> = None;
    let mut current_hunk: Option<ParsedGitDiffHunk> = None;

    let mut lines = combined.lines().chain(patches.lines());
    while let Some(line) = lines.next() {
        if line.starts_with("* Unmerged path ") {
            let path = paths
                .pop_front()
                .ok_or_else(|| "unmerged Git marker has no path record".to_owned())?;
            // Staged conflicts have no patch. Their marker prints the raw
            // name, including embedded newlines, rather than Git quoting.
            for _ in path.bytes().filter(|byte| *byte == b'\n') {
                lines
                    .next()
                    .ok_or_else(|| "truncated unmerged Git marker".to_owned())?;
            }
            continue;
        }
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
                relative_path: decode_git_patch_path(relative_path)?,
                header_lines: vec![line.to_owned()],
                unmerged: true,
                hunks: Vec::new(),
            });
            continue;
        }

        if line.starts_with("diff --git ") {
            // A type change has one numstat record but a delete/add patch
            // pair with identical headers. Both belong to that same delta.
            let type_change_path = current_file.as_ref().and_then(|file| {
                (file
                    .header_lines
                    .first()
                    .is_some_and(|header| header == line)
                    && file
                        .header_lines
                        .iter()
                        .any(|header| header.starts_with("deleted file mode "))
                    && lines
                        .clone()
                        .next()
                        .is_some_and(|header| header.starts_with("new file mode ")))
                .then(|| file.relative_path.clone())
            });
            finish_parsed_git_diff_file(
                &mut files,
                &mut current_file,
                &mut current_hunk,
                "hunk appeared before file in git diff",
            )?;

            let relative_path = match type_change_path {
                Some(path) => path,
                None => paths
                    .pop_front()
                    .ok_or_else(|| "Git patch has no path record".to_owned())?
                    .to_owned(),
            };
            current_file = Some(ParsedGitDiffFile {
                relative_path,
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
    if !paths.is_empty() {
        return Err("Git path records have no matching patches".to_owned());
    }

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

fn decode_git_patch_path(path: &str) -> Result<String, String> {
    if !path.starts_with('"') {
        return Ok(path.to_owned());
    }
    let quoted = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .ok_or_else(|| "unterminated quoted Git patch path".to_owned())?;
    let mut bytes = quoted.bytes();
    let mut decoded = Vec::with_capacity(quoted.len());
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let escaped = match bytes.next() {
            Some(b'a') => 7,
            Some(b'b') => 8,
            Some(b't') => b'\t',
            Some(b'n') => b'\n',
            Some(b'v') => 11,
            Some(b'f') => 12,
            Some(b'r') => b'\r',
            Some(b'\\') => b'\\',
            Some(b'"') => b'"',
            Some(first @ b'0'..=b'3') => {
                let second = bytes.next().filter(|byte| matches!(byte, b'0'..=b'7'));
                let third = bytes.next().filter(|byte| matches!(byte, b'0'..=b'7'));
                match (second, third) {
                    (Some(second), Some(third)) => {
                        (first - b'0') * 64 + (second - b'0') * 8 + (third - b'0')
                    }
                    _ => return Err("invalid Git patch path octal escape".to_owned()),
                }
            }
            _ => return Err("invalid Git patch path escape".to_owned()),
        };
        decoded.push(escaped);
    }
    String::from_utf8(decoded).map_err(|_| "Git patch path is not valid UTF-8".to_owned())
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
