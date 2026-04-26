use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use protocol::{
    CommandErrorCode, CommandErrorPayload, DiffContextMode, FileEntryOp, FrameKind, Project,
    ProjectDiffScope, ProjectFileContentsPayload, ProjectFileEntry, ProjectFileKind,
    ProjectFileListPayload, ProjectGitChangeKind, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload, ProjectGitFileStatus,
    ProjectGitStatusPayload, ProjectId, ProjectPath, ProjectReadDiffPayload,
    ProjectReadFilePayload, ProjectRootGitStatus, ProjectRootListing, ProjectRootPath, StreamPath,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};

use crate::store::project::ProjectStore;
use crate::stream::Stream;

const PROJECT_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);
const GIT_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A (relative_path, kind) pair used for comparing file listings between snapshots.
pub(crate) type RawFileEntry = (String, ProjectFileKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitAccessMode {
    ReadOnly,
    Mutating,
}

#[derive(Debug, Default)]
pub(crate) struct ProjectSnapshotState {
    /// Previous file entries per root, used to decide whether a new full snapshot is needed.
    pub file_entries: BTreeMap<ProjectRootPath, BTreeSet<RawFileEntry>>,
    pub git_status: Option<Value>,
    pub diff_context_modes: HashMap<(StreamPath, ProjectDiffRequestKey), DiffContextMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProjectDiffRequestKey {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
}

pub(crate) struct ProjectStreamSubscription {
    pub task: JoinHandle<()>,
    pub handle: ProjectStreamHandle,
}

#[derive(Clone)]
pub(crate) struct ProjectStreamHandle {
    tx: mpsc::Sender<ProjectStreamCommand>,
}

enum ProjectStreamCommand {
    AddSubscriber {
        host_path: StreamPath,
        stream: Stream,
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
    ) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::AddSubscriber {
                host_path,
                stream,
                reply,
            })
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }

    pub(crate) async fn remove_subscriber(&self, host_path: StreamPath) {
        let _ = self
            .tx
            .send(ProjectStreamCommand::RemoveSubscriber { host_path })
            .await;
    }

    pub(crate) async fn refresh(&self) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(ProjectStreamCommand::Refresh { reply })
            .await
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
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?;
        response
            .await
            .map_err(|_| "project stream subscription stopped".to_owned())?
    }
}

pub(crate) async fn spawn_project_subscription(
    project_store: Arc<Mutex<ProjectStore>>,
    project_id: ProjectId,
) -> Result<ProjectStreamSubscription, String> {
    let project = load_subscription_project(&project_store, &project_id).await?;
    let (watch_tx, watch_rx) = mpsc::unbounded_channel();
    let watcher = create_project_watcher(&project, watch_tx.clone())?;
    let watched_roots = project.roots.clone();
    let snapshot = initialize_snapshot(&project)?;
    let (command_tx, command_rx) = mpsc::channel(16);
    let handle = ProjectStreamHandle { tx: command_tx };

    let task = tokio::spawn(async move {
        run_project_subscription(
            project_store,
            project_id,
            project,
            snapshot,
            watcher,
            watched_roots,
            watch_tx,
            watch_rx,
            command_rx,
        )
        .await;
    });

    Ok(ProjectStreamSubscription { task, handle })
}

async fn load_subscription_project(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
) -> Result<Project, String> {
    let projects = project_store.lock().await.list()?;
    projects
        .into_iter()
        .find(|project| &project.id == project_id)
        .ok_or_else(|| format!("project {} disappeared while stream was active", project_id))
}

fn create_project_watcher(
    project: &Project,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
) -> Result<RecommendedWatcher, String> {
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = watch_tx.send(result);
        },
        Config::default(),
    )
    .map_err(|error| format!("failed to create project filesystem watcher: {error}"))?;

    for root in &project.roots {
        watcher
            .watch(Path::new(root), RecursiveMode::Recursive)
            .map_err(|error| format!("failed to watch project root '{}': {error}", root))?;
    }

    Ok(watcher)
}

fn initialize_snapshot(project: &Project) -> Result<ProjectSnapshotState, String> {
    let mut snapshot = ProjectSnapshotState {
        file_entries: scan_raw_entries(project)?,
        ..Default::default()
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
    mut watcher: RecommendedWatcher,
    mut watched_roots: Vec<String>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    mut watch_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    mut command_rx: mpsc::Receiver<ProjectStreamCommand>,
) {
    let mut subscribers = HashMap::<StreamPath, Stream>::new();
    let mut pending_update = PendingProjectUpdate::default();
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
                    ProjectStreamCommand::AddSubscriber { host_path, stream, reply } => {
                        use std::collections::hash_map::Entry;
                        match subscribers.entry(host_path.clone()) {
                            Entry::Occupied(mut e) => {
                                e.insert(stream);
                                let _ = reply.send(Ok(()));
                                continue;
                            }
                            Entry::Vacant(e) => {
                                e.insert(stream.clone());
                            }
                        }
                        let result = emit_snapshot_to_stream(&stream, &project, &snapshot).await;
                        if result.is_err() {
                            subscribers.remove(&host_path);
                            snapshot.diff_context_modes.retain(|(subscriber, _), _| subscriber != &host_path);
                        }
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::RemoveSubscriber { host_path } => {
                        subscribers.remove(&host_path);
                        snapshot.diff_context_modes.retain(|(subscriber, _), _| subscriber != &host_path);
                    }
                    ProjectStreamCommand::Refresh { reply } => {
                        let result = refresh_full(
                            &project_store,
                            &project_id,
                            &mut project,
                            &mut snapshot,
                            &mut watcher,
                            &mut watched_roots,
                            watch_tx.clone(),
                            &mut subscribers,
                        ).await;
                        let _ = reply.send(result);
                    }
                    ProjectStreamCommand::RememberDiffContext { host_path, key, context_mode, reply } => {
                        snapshot.diff_context_modes.insert((host_path, key), context_mode);
                        let _ = reply.send(Ok(()));
                    }
                }
            }
            maybe_event = watch_rx.recv() => {
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
                        let refresh = classify_watch_event(&event);
                        if !refresh.is_empty() {
                            pending_update.merge(refresh);
                            debounce_active = true;
                            debounce_sleep.as_mut().reset(Instant::now() + PROJECT_REFRESH_DEBOUNCE);
                        }
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
                if let Err(error) = refresh_incremental(
                    &project_store,
                    &project_id,
                    &mut project,
                    &mut snapshot,
                    &mut watcher,
                    &mut watched_roots,
                    watch_tx.clone(),
                    &mut subscribers,
                    refresh.files,
                    refresh.git,
                ).await {
                    tracing::warn!(project_id = %project_id, error = %error, "stopping project subscription after debounced refresh failure");
                    emit_fatal_project_stream_error(&mut subscribers, "project_watch", error).await;
                    return;
                }
            }
            _ = git_poll.tick() => {
                if let Err(error) = refresh_incremental(
                    &project_store,
                    &project_id,
                    &mut project,
                    &mut snapshot,
                    &mut watcher,
                    &mut watched_roots,
                    watch_tx.clone(),
                    &mut subscribers,
                    false,
                    true,
                ).await {
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
    watcher: &mut RecommendedWatcher,
    watched_roots: &mut Vec<String>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    subscribers: &mut HashMap<StreamPath, Stream>,
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

    fan_out_payload(subscribers, FrameKind::ProjectFileList, &file_list).await?;
    fan_out_payload(subscribers, FrameKind::ProjectGitStatus, &git_status).await?;
    refresh_remembered_diffs(project, snapshot, subscribers).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn refresh_incremental(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    project: &mut Project,
    snapshot: &mut ProjectSnapshotState,
    watcher: &mut RecommendedWatcher,
    watched_roots: &mut Vec<String>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
    subscribers: &mut HashMap<StreamPath, Stream>,
    files_changed: bool,
    git_changed: bool,
) -> Result<(), String> {
    if !files_changed && !git_changed {
        return Ok(());
    }

    let latest_project = load_subscription_project(project_store, project_id).await?;
    ensure_watched_roots(&latest_project, watcher, watched_roots, watch_tx)?;
    *project = latest_project;

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
            refresh_remembered_diffs(project, snapshot, subscribers).await;
        }
    }

    Ok(())
}

fn ensure_watched_roots(
    project: &Project,
    watcher: &mut RecommendedWatcher,
    watched_roots: &mut Vec<String>,
    watch_tx: mpsc::UnboundedSender<notify::Result<Event>>,
) -> Result<(), String> {
    if *watched_roots == project.roots {
        return Ok(());
    }

    *watcher = create_project_watcher(project, watch_tx)?;
    *watched_roots = project.roots.clone();
    Ok(())
}

fn classify_watch_event(event: &Event) -> PendingProjectUpdate {
    let mut refresh = PendingProjectUpdate::default();
    for path in &event.paths {
        if is_git_head_or_index(path) {
            refresh.git = true;
        } else if !is_inside_git(path) {
            refresh.files = true;
            refresh.git = true;
        }
    }
    refresh
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
        .roots
        .iter()
        .map(|root| {
            let root = ProjectRootPath(root.clone());
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

fn serialize_git_status(git_status: &ProjectGitStatusPayload) -> Result<Value, String> {
    serde_json::to_value(git_status)
        .map_err(|error| format!("failed to serialize project git status: {error}"))
}

async fn emit_snapshot_to_stream(
    stream: &Stream,
    project: &Project,
    snapshot: &ProjectSnapshotState,
) -> Result<(), String> {
    let file_list = full_file_list_from_raw(project, &snapshot.file_entries);
    send_payload(stream, FrameKind::ProjectFileList, &file_list).await?;
    let Some(git_status) = snapshot.git_status.clone() else {
        return Err("project git status snapshot was not initialized".to_owned());
    };
    stream
        .send_value(FrameKind::ProjectGitStatus, git_status)
        .await
        .map_err(|_| "project stream closed".to_owned())
}

async fn fan_out_payload<T: serde::Serialize>(
    subscribers: &mut HashMap<StreamPath, Stream>,
    kind: FrameKind,
    payload: &T,
) -> Result<(), String> {
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("failed to serialize {kind} payload: {error}"))?;
    fan_out_value(subscribers, kind, payload).await;
    Ok(())
}

async fn fan_out_value(
    subscribers: &mut HashMap<StreamPath, Stream>,
    kind: FrameKind,
    payload: Value,
) {
    let mut dead = Vec::new();
    for (host_path, stream) in subscribers.iter() {
        if stream.send_value(kind, payload.clone()).await.is_err() {
            dead.push(host_path.clone());
        }
    }
    for host_path in dead {
        subscribers.remove(&host_path);
    }
}

async fn refresh_remembered_diffs(
    project: &Project,
    snapshot: &ProjectSnapshotState,
    subscribers: &HashMap<StreamPath, Stream>,
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
        let Some(stream) = subscribers.get(&host_path) else {
            continue;
        };
        let payload = ProjectReadDiffPayload {
            root: key.root.clone(),
            scope: key.scope,
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
        .await
        .map_err(|_| "project stream closed".to_owned())
}

async fn emit_fatal_project_stream_error(
    subscribers: &mut HashMap<StreamPath, Stream>,
    operation: &str,
    message: String,
) {
    let streams = subscribers.values().cloned().collect::<Vec<_>>();
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

async fn emit_project_command_error(
    stream: &Stream,
    request_kind: FrameKind,
    operation: &str,
    message: String,
    fatal: bool,
) {
    let payload = CommandErrorPayload {
        stream: stream.path().clone(),
        request_kind,
        operation: operation.to_owned(),
        code: CommandErrorCode::Internal,
        message,
        fatal,
    };
    match serde_json::to_value(payload) {
        Ok(payload) => {
            let _ = stream.send_value(FrameKind::CommandError, payload).await;
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
    for root in &project.roots {
        let root_path = Path::new(root);
        let metadata = fs::metadata(root_path)
            .map_err(|err| format!("Failed to stat project root '{}': {err}", root))?;
        if !metadata.is_dir() {
            return Err(format!("Project root '{}' is not a directory", root));
        }
        let mut raw = Vec::new();
        collect_raw_entries(root_path, root_path, &mut raw, 0, max_depth)?;
        result.insert(ProjectRootPath(root.clone()), raw.into_iter().collect());
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

fn build_git_status_with_runner<F>(
    project: &Project,
    mut run_git: F,
) -> Result<ProjectGitStatusPayload, String>
where
    F: FnMut(&str, &[&str], GitAccessMode) -> Result<String, String>,
{
    let mut roots = Vec::with_capacity(project.roots.len());

    for root in &project.roots {
        let output = match run_git(
            root,
            &["status", "--porcelain=v2", "--branch"],
            GitAccessMode::ReadOnly,
        ) {
            Ok(output) => output,
            Err(err) if err.contains("not a git repository") => {
                roots.push(ProjectRootGitStatus {
                    root: ProjectRootPath(root.clone()),
                    branch: None,
                    ahead: 0,
                    behind: 0,
                    clean: true,
                    files: Vec::new(),
                });
                continue;
            }
            Err(err) => return Err(err),
        };
        let mut branch = None;
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

        roots.push(ProjectRootGitStatus {
            root: ProjectRootPath(root.clone()),
            branch,
            ahead,
            behind,
            clean: files.is_empty(),
            files: files.into_values().collect(),
        });
    }

    Ok(ProjectGitStatusPayload { roots })
}

pub(crate) fn read_file(
    project: &Project,
    payload: ProjectReadFilePayload,
) -> Result<ProjectFileContentsPayload, String> {
    let path = normalize_read_path(project, payload.path)?;
    validate_project_path(project, &path)?;
    let absolute = absolute_project_path(&path)?;
    let bytes = fs::read(&absolute)
        .map_err(|err| format!("Failed to read file '{}': {err}", absolute.display()))?;
    match String::from_utf8(bytes) {
        Ok(contents) => Ok(ProjectFileContentsPayload {
            path,
            contents: Some(contents),
            is_binary: false,
        }),
        Err(_) => Ok(ProjectFileContentsPayload {
            path,
            contents: None,
            is_binary: true,
        }),
    }
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

    let mut args = vec!["diff", git_diff_context_arg(payload.context_mode)];
    if matches!(payload.scope, ProjectDiffScope::Staged) {
        args.push("--cached");
    }
    if let Some(path) = &payload.path {
        args.push("--");
        args.push(path);
    }

    let raw = run_git(&payload.root.0, &args, GitAccessMode::ReadOnly)?;
    let mut files = parse_git_diff(&raw)?;
    if matches!(payload.scope, ProjectDiffScope::Unstaged) {
        let untracked_paths = list_untracked_paths_with_runner(
            &payload.root.0,
            payload.path.as_deref(),
            &mut run_git,
        )?;
        for relative_path in untracked_paths {
            files.push(build_untracked_diff_file(&payload.root.0, &relative_path)?);
        }
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    }

    Ok(ProjectGitDiffPayload {
        root: payload.root,
        scope: payload.scope,
        path: payload.path,
        context_mode: payload.context_mode,
        files,
    })
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
    let contents = match fs::read_to_string(&absolute_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(format!("binary file: {}", relative_path));
        }
        Err(error) => {
            return Err(format!(
                "Failed to read untracked file '{}' from '{}': {error}",
                relative_path, root
            ));
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
    if project.roots.iter().any(|candidate| candidate == &root.0) {
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

    for root in &project.roots {
        let Ok(relative) = absolute.strip_prefix(root) else {
            continue;
        };
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        if relative_path.is_empty() {
            return None;
        }
        return Some(ProjectPath {
            root: ProjectRootPath(root.clone()),
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
            Ok(ProjectGitDiffFile {
                relative_path: relative_path.clone(),
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

fn parse_raw_git_diff(raw: &str) -> Result<Vec<ParsedGitDiffFile>, String> {
    let mut files = Vec::new();
    let mut current_file: Option<ParsedGitDiffFile> = None;
    let mut current_hunk: Option<ParsedGitDiffHunk> = None;

    for line in raw.lines() {
        if let Some(diff_line) = line.strip_prefix("diff --git ") {
            if let Some(hunk) = current_hunk.take() {
                let file = current_file
                    .as_mut()
                    .ok_or_else(|| "hunk appeared before file in git diff".to_owned())?;
                file.hunks.push(hunk);
            }
            if let Some(file) = current_file.take() {
                files.push(file);
            }

            let parts: Vec<&str> = diff_line.split_whitespace().collect();
            if parts.len() != 2 {
                return Err(format!("invalid diff header '{}'", line));
            }
            current_file = Some(ParsedGitDiffFile {
                relative_path: parse_diff_path(parts[0], parts[1]),
                header_lines: vec![line.to_owned()],
                hunks: Vec::new(),
            });
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

    if let Some(hunk) = current_hunk.take() {
        let file = current_file
            .as_mut()
            .ok_or_else(|| "trailing hunk appeared before file in git diff".to_owned())?;
        file.hunks.push(hunk);
    }
    if let Some(file) = current_file.take() {
        files.push(file);
    }

    Ok(files)
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

#[cfg(test)]
mod tests {
    use super::*;

    use protocol::{DiffContextMode, ProjectDiffScope, ProjectId};
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn test_project(root: &str) -> Project {
        Project {
            id: ProjectId("project-1".to_owned()),
            name: "Project".to_owned(),
            roots: vec![root.to_owned()],
            sort_order: 0,
        }
    }

    #[test]
    fn build_git_status_uses_read_only_git_access() {
        let project = test_project("/repo");
        let mut calls = Vec::new();

        let status = build_git_status_with_runner(&project, |root, args, access_mode| {
            calls.push((
                root.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                access_mode,
            ));
            Ok("# branch.oid abc123\n# branch.head main\n".to_owned())
        })
        .expect("build_git_status should succeed");

        assert_eq!(
            calls,
            vec![(
                "/repo".to_owned(),
                vec![
                    "status".to_owned(),
                    "--porcelain=v2".to_owned(),
                    "--branch".to_owned(),
                ],
                GitAccessMode::ReadOnly,
            )]
        );
        assert_eq!(status.roots.len(), 1);
        assert_eq!(status.roots[0].branch.as_deref(), Some("main"));
        assert!(status.roots[0].clean);
    }

    #[test]
    fn read_diff_uses_read_only_git_access() {
        let project = test_project("/repo");
        let mut calls = Vec::new();

        let diff = read_diff_with_runner(
            &project,
            ProjectReadDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Unstaged,
                path: Some("src/lib.rs".to_owned()),
                context_mode: DiffContextMode::Hunks,
            },
            |root, args, access_mode| {
                calls.push((
                    root.to_owned(),
                    args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                    access_mode,
                ));
                Ok(String::new())
            },
        )
        .expect("read_diff should succeed");

        assert_eq!(
            calls,
            vec![
                (
                    "/repo".to_owned(),
                    vec![
                        "diff".to_owned(),
                        "-U3".to_owned(),
                        "--".to_owned(),
                        "src/lib.rs".to_owned(),
                    ],
                    GitAccessMode::ReadOnly,
                ),
                (
                    "/repo".to_owned(),
                    vec![
                        "ls-files".to_owned(),
                        "--others".to_owned(),
                        "--exclude-standard".to_owned(),
                        "--".to_owned(),
                        "src/lib.rs".to_owned(),
                    ],
                    GitAccessMode::ReadOnly,
                ),
            ]
        );
        assert_eq!(diff.root.0, "/repo");
        assert_eq!(diff.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(diff.context_mode, DiffContextMode::Hunks);
    }

    #[test]
    fn list_untracked_paths_returns_correct_files() {
        let mut calls = Vec::new();

        let paths = list_untracked_paths_with_runner(
            "/repo",
            Some("src"),
            &mut |root, args, access_mode| {
                calls.push((
                    root.to_owned(),
                    args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                    access_mode,
                ));
                Ok("src/new.rs\nsrc/other.rs\n".to_owned())
            },
        )
        .expect("list_untracked_paths_with_runner should succeed");

        assert_eq!(paths, vec!["src/new.rs", "src/other.rs"]);
        assert_eq!(
            calls,
            vec![(
                "/repo".to_owned(),
                vec![
                    "ls-files".to_owned(),
                    "--others".to_owned(),
                    "--exclude-standard".to_owned(),
                    "--".to_owned(),
                    "src".to_owned(),
                ],
                GitAccessMode::ReadOnly,
            )]
        );
    }

    #[test]
    fn build_untracked_diff_file_all_added_lines() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let path = temp_dir.path().join("src").join("new.rs");
        fs::create_dir_all(path.parent().expect("new.rs parent")).expect("create parent");
        fs::write(&path, "first\nsecond\nthird\n").expect("write untracked file");

        let diff = build_untracked_diff_file(
            temp_dir.path().to_str().expect("tempdir path utf8"),
            "src/new.rs",
        )
        .expect("build_untracked_diff_file should succeed");

        assert_eq!(diff.relative_path, "src/new.rs");
        assert_eq!(diff.hunks.len(), 1);
        let hunk = &diff.hunks[0];
        assert_eq!(hunk.hunk_id, "src/new.rs::0");
        assert_eq!(hunk.old_start, 0);
        assert_eq!(hunk.old_count, 0);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.new_count, 3);
        assert_eq!(hunk.lines.len(), 3);
        for (index, line) in hunk.lines.iter().enumerate() {
            assert_eq!(line.kind, ProjectGitDiffLineKind::Added);
            assert_eq!(line.old_line_number, None);
            assert_eq!(line.new_line_number, Some((index + 1) as u32));
        }
        assert_eq!(hunk.lines[0].text, "first");
        assert_eq!(hunk.lines[1].text, "second");
        assert_eq!(hunk.lines[2].text, "third");
    }

    #[test]
    fn build_untracked_diff_file_missing_file_returns_err() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");

        let error = build_untracked_diff_file(
            temp_dir.path().to_str().expect("tempdir path utf8"),
            "src/missing.rs",
        )
        .expect_err("missing file should error");

        assert!(error.contains("Failed to read untracked file 'src/missing.rs'"));
    }

    #[test]
    fn build_untracked_diff_file_binary_returns_err() {
        let temp_dir = tempfile::tempdir().expect("create tempdir");
        let path = temp_dir.path().join("binary.dat");
        fs::write(&path, [0xff_u8, 0xfe_u8, 0x00_u8]).expect("write binary file");

        let error = build_untracked_diff_file(
            temp_dir.path().to_str().expect("tempdir path utf8"),
            "binary.dat",
        )
        .expect_err("binary file should error");

        assert_eq!(error, "binary file: binary.dat");
    }

    #[test]
    fn parse_git_diff_emits_prefix_free_text_and_typed_line_numbers() {
        let raw = "\
diff --git a/src/lib.rs b/src/lib.rs\n\
index 83db48f..f735c2b 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -1,3 +1,4 @@\n\
 line1\n\
-line2\n\
+line2 changed\n\
+line2 extra\n\
 line3\n";

        let files = parse_git_diff(raw).expect("parse_git_diff should succeed");
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.relative_path, "src/lib.rs");
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.old_count, 3);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.new_count, 4);

        assert_eq!(hunk.lines.len(), 5);
        assert_eq!(hunk.lines[0].text, "line1");
        assert_eq!(hunk.lines[0].old_line_number, Some(1));
        assert_eq!(hunk.lines[0].new_line_number, Some(1));

        assert_eq!(hunk.lines[1].text, "line2");
        assert_eq!(hunk.lines[1].old_line_number, Some(2));
        assert_eq!(hunk.lines[1].new_line_number, None);

        assert_eq!(hunk.lines[2].text, "line2 changed");
        assert_eq!(hunk.lines[2].old_line_number, None);
        assert_eq!(hunk.lines[2].new_line_number, Some(2));

        assert_eq!(hunk.lines[3].text, "line2 extra");
        assert_eq!(hunk.lines[3].old_line_number, None);
        assert_eq!(hunk.lines[3].new_line_number, Some(3));

        assert_eq!(hunk.lines[4].text, "line3");
        assert_eq!(hunk.lines[4].old_line_number, Some(3));
        assert_eq!(hunk.lines[4].new_line_number, Some(4));
    }

    #[cfg(unix)]
    struct FakeGitBinary {
        dir: PathBuf,
        binary: PathBuf,
        log_path: PathBuf,
    }

    #[cfg(unix)]
    impl FakeGitBinary {
        fn new(stdout: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("tyde-project-stream-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("create fake git dir");

            let binary = dir.join("git");
            let stdout_path = dir.join("stdout.txt");
            let log_path = dir.join("args.log");

            fs::write(&stdout_path, stdout).expect("write fake git stdout");
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\n",
                    log_path.display(),
                    stdout_path.display()
                ),
            )
            .expect("write fake git script");

            let mut permissions = fs::metadata(&binary)
                .expect("stat fake git script")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&binary, permissions).expect("chmod fake git script");

            Self {
                dir,
                binary,
                log_path,
            }
        }

        fn binary_path(&self) -> &Path {
            &self.binary
        }

        fn logged_args(&self) -> Vec<String> {
            fs::read_to_string(&self.log_path)
                .expect("read fake git log")
                .lines()
                .map(|line| line.to_owned())
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeGitBinary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_git_commands_disable_optional_locks() {
        let fake_git = FakeGitBinary::new("# branch.oid abc123\n# branch.head main\n");

        let output = run_git_mode_with_binary(
            fake_git.binary_path(),
            "/repo",
            &["status", "--porcelain=v2", "--branch"],
            GitAccessMode::ReadOnly,
        )
        .expect("read-only git command should succeed");

        assert!(output.contains("# branch.head main"));
        assert_eq!(
            fake_git.logged_args(),
            vec![
                "--no-optional-locks".to_owned(),
                "-C".to_owned(),
                "/repo".to_owned(),
                "status".to_owned(),
                "--porcelain=v2".to_owned(),
                "--branch".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn mutating_git_commands_do_not_disable_locks() {
        let fake_git = FakeGitBinary::new("");

        run_git_mode_with_binary(
            fake_git.binary_path(),
            "/repo",
            &["add", "--", "src/lib.rs"],
            GitAccessMode::Mutating,
        )
        .expect("mutating git command should succeed");

        assert_eq!(
            fake_git.logged_args(),
            vec![
                "-C".to_owned(),
                "/repo".to_owned(),
                "add".to_owned(),
                "--".to_owned(),
                "src/lib.rs".to_owned(),
            ]
        );
    }
}
