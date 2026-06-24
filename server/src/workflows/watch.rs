use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep};

const WORKFLOW_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);
const MISSING_TARGET_RETRY: Duration = Duration::from_millis(500);
const WATCH_OVERFLOW_POLL: Duration = Duration::from_millis(100);
const WORKFLOW_WATCH_COMMAND_CAPACITY: usize = 8;
const WORKFLOW_WATCH_EVENT_CAPACITY: usize = 128;
const WORKFLOW_SIGNAL_CAPACITY: usize = 1;

#[derive(Debug, Clone)]
pub(crate) enum WorkflowCatalogSignal {
    Rescan { reason: String },
    WatcherError { message: String },
}

#[derive(Clone, Debug)]
pub(crate) struct WorkflowWatcherHandle {
    tx: mpsc::Sender<WorkflowWatcherCommand>,
}

enum WorkflowWatcherCommand {
    SetTargets {
        targets: Vec<PathBuf>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

struct WorkflowWatcher {
    inner: Option<RecommendedWatcher>,
}

impl WorkflowWatcher {
    fn new(inner: RecommendedWatcher) -> Self {
        Self { inner: Some(inner) }
    }
}

impl Drop for WorkflowWatcher {
    fn drop(&mut self) {
        let Some(watcher) = self.inner.take() else {
            return;
        };
        match std::thread::Builder::new()
            .name("tyde-workflow-watch-drop".to_owned())
            .spawn(move || drop(watcher))
        {
            Ok(_) => {}
            Err(error) => tracing::warn!(
                %error,
                "failed to spawn workflow watcher drop thread; dropping watcher inline"
            ),
        }
    }
}

impl WorkflowWatcherHandle {
    pub(crate) async fn set_targets(&self, targets: Vec<PathBuf>) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(WorkflowWatcherCommand::SetTargets { targets, reply })
            .await
            .map_err(|_| "workflow watcher stopped".to_owned())?;
        response
            .await
            .map_err(|_| "workflow watcher stopped".to_owned())?
    }
}

pub(crate) fn spawn_workflow_watcher(
    initial_targets: Vec<PathBuf>,
    signal_tx: mpsc::Sender<WorkflowCatalogSignal>,
) -> WorkflowWatcherHandle {
    let (command_tx, command_rx) = mpsc::channel(WORKFLOW_WATCH_COMMAND_CAPACITY);
    let (watch_tx, watch_rx) = mpsc::channel(WORKFLOW_WATCH_EVENT_CAPACITY);
    let watch_overflow = Arc::new(AtomicBool::new(false));
    let worker = run_workflow_watcher(
        initial_targets,
        command_rx,
        watch_tx,
        watch_rx,
        signal_tx,
        watch_overflow,
    );
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
    } else if let Err(error) = std::thread::Builder::new()
        .name("tyde-workflow-watch".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build workflow watcher runtime");
            runtime.block_on(worker);
        })
    {
        tracing::warn!(%error, "failed to spawn workflow watcher thread");
    }
    WorkflowWatcherHandle { tx: command_tx }
}

pub(crate) const fn workflow_signal_capacity() -> usize {
    WORKFLOW_SIGNAL_CAPACITY
}

async fn run_workflow_watcher(
    initial_targets: Vec<PathBuf>,
    mut command_rx: mpsc::Receiver<WorkflowWatcherCommand>,
    watch_tx: mpsc::Sender<notify::Result<Event>>,
    mut watch_rx: mpsc::Receiver<notify::Result<Event>>,
    signal_tx: mpsc::Sender<WorkflowCatalogSignal>,
    watch_overflow: Arc<AtomicBool>,
) {
    let mut targets = normalize_targets(initial_targets);
    let mut missing_targets = BTreeSet::new();
    let mut watcher = rebuild_watcher(
        &targets,
        watch_tx.clone(),
        Arc::clone(&watch_overflow),
        &signal_tx,
        &mut missing_targets,
    )
    .await;
    let mut debounce_active = false;
    let mut debounce_sleep = Box::pin(sleep(WORKFLOW_REFRESH_DEBOUNCE));
    let mut missing_tick = interval_at(Instant::now() + MISSING_TARGET_RETRY, MISSING_TARGET_RETRY);
    missing_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut overflow_tick = interval_at(Instant::now() + WATCH_OVERFLOW_POLL, WATCH_OVERFLOW_POLL);
    overflow_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else {
                    return;
                };
                match command {
                    WorkflowWatcherCommand::SetTargets { targets: new_targets, reply } => {
                        let new_targets = normalize_targets(new_targets);
                        let missing_target_now_exists = missing_targets
                            .iter()
                            .any(|target| target.is_dir());
                        if new_targets != targets || missing_target_now_exists {
                            targets = new_targets;
                            watcher = rebuild_watcher(
                                &targets,
                                watch_tx.clone(),
                                Arc::clone(&watch_overflow),
                                &signal_tx,
                                &mut missing_targets,
                            ).await;
                        }
                        let _ = reply.send(Ok(()));
                    }
                }
            }
            maybe_event = watch_rx.recv() => {
                let Some(event_result) = maybe_event else {
                    send_error_signal(
                        &signal_tx,
                        "workflow filesystem watcher stopped unexpectedly".to_owned(),
                    ).await;
                    return;
                };
                match event_result {
                    Ok(event) => {
                        if event_mentions_markdown(&event) {
                            debounce_active = true;
                            debounce_sleep.as_mut().reset(Instant::now() + WORKFLOW_REFRESH_DEBOUNCE);
                        }
                    }
                    Err(error) => {
                        send_error_signal(
                            &signal_tx,
                            format!("workflow filesystem watcher failed: {error}"),
                        ).await;
                    }
                }
            }
            _ = &mut debounce_sleep, if debounce_active => {
                debounce_active = false;
                try_send_rescan_signal(&signal_tx, "workflow_fs_watch");
            }
            _ = missing_tick.tick(), if !missing_targets.is_empty() => {
                let now_existing = missing_targets
                    .iter()
                    .any(|target| target.is_dir());
                if now_existing {
                    watcher = rebuild_watcher(
                        &targets,
                        watch_tx.clone(),
                        Arc::clone(&watch_overflow),
                        &signal_tx,
                        &mut missing_targets,
                    ).await;
                    try_send_rescan_signal(&signal_tx, "workflow_watch_target_created");
                }
            }
            _ = overflow_tick.tick() => {
                if watch_overflow.swap(false, Ordering::AcqRel) {
                    debounce_active = true;
                    debounce_sleep.as_mut().reset(Instant::now() + WORKFLOW_REFRESH_DEBOUNCE);
                }
            }
        }
        let _keep_watcher_alive = watcher.as_ref();
    }
}

fn normalize_targets(targets: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut targets = targets
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    targets.sort();
    targets
}

async fn rebuild_watcher(
    targets: &[PathBuf],
    watch_tx: mpsc::Sender<notify::Result<Event>>,
    watch_overflow: Arc<AtomicBool>,
    signal_tx: &mpsc::Sender<WorkflowCatalogSignal>,
    missing_targets: &mut BTreeSet<PathBuf>,
) -> Option<WorkflowWatcher> {
    missing_targets.clear();
    let mut watcher = match RecommendedWatcher::new(
        move |result| {
            if watch_tx.try_send(result).is_err() {
                watch_overflow.store(true, Ordering::Release);
            }
        },
        Config::default(),
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            send_error_signal(
                signal_tx,
                format!("failed to create workflow filesystem watcher: {error}"),
            )
            .await;
            return None;
        }
    };

    for target in targets {
        if !target.is_dir() {
            missing_targets.insert(target.clone());
            continue;
        }
        if let Err(error) = watcher.watch(target, RecursiveMode::Recursive) {
            missing_targets.insert(target.clone());
            send_error_signal(
                signal_tx,
                format!(
                    "failed to watch workflow directory '{}': {error}",
                    target.display()
                ),
            )
            .await;
        }
    }

    Some(WorkflowWatcher::new(watcher))
}

fn try_send_rescan_signal(signal_tx: &mpsc::Sender<WorkflowCatalogSignal>, reason: &'static str) {
    match signal_tx.try_send(WorkflowCatalogSignal::Rescan {
        reason: reason.to_owned(),
    }) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!("workflow catalog signal receiver is closed");
        }
    }
}

async fn send_error_signal(signal_tx: &mpsc::Sender<WorkflowCatalogSignal>, message: String) {
    if signal_tx
        .send(WorkflowCatalogSignal::WatcherError { message })
        .await
        .is_err()
    {
        tracing::warn!("workflow catalog signal receiver is closed");
    }
}

fn event_mentions_markdown(event: &Event) -> bool {
    event.paths.iter().any(|path| is_markdown_path(path))
}

fn is_markdown_path(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("md")
}
