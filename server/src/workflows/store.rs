use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use devtools_protocol::WORKFLOW_RUN_STORE_PATH_ENV;
use protocol::{ProjectId, WorkflowRunId, WorkflowRunSnapshot, WorkflowRunSnapshotStatus};
use serde::{Deserialize, Serialize};

const STORE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    runs: HashMap<WorkflowRunId, WorkflowRunSnapshot>,
}

#[derive(Debug)]
pub(crate) struct WorkflowRunStore {
    path: PathBuf,
    runs: HashMap<WorkflowRunId, WorkflowRunSnapshot>,
}

impl WorkflowRunStore {
    pub(crate) fn default_path() -> Result<PathBuf, String> {
        let override_path = std::env::var(WORKFLOW_RUN_STORE_PATH_ENV).ok();
        if let Some(path) = resolve_override_path(override_path.as_deref()) {
            return Ok(path);
        }
        Ok(default_path_for_home(&crate::paths::home_dir()?))
    }

    pub(crate) fn load(path: PathBuf) -> Result<Self, String> {
        let mut runs = read_from_disk(&path)?;
        let now = crate::agent::now_ms();
        let mut changed = false;
        for run in runs.values_mut() {
            if run.status == WorkflowRunSnapshotStatus::Running {
                run.status = WorkflowRunSnapshotStatus::Failed;
                run.error = Some("Workflow host restarted while this run was in flight".to_owned());
                run.updated_at_ms = now;
                run.completed_at_ms = Some(now);
                changed = true;
            }
        }
        let store = Self { path, runs };
        if changed {
            store.save_current()?;
        }
        Ok(store)
    }

    pub(crate) fn list(&self) -> Vec<WorkflowRunSnapshot> {
        let mut runs = self.runs.values().cloned().collect::<Vec<_>>();
        runs.sort_by_key(|run| std::cmp::Reverse(run.updated_at_ms));
        runs
    }

    pub(crate) fn get(&self, id: &WorkflowRunId) -> Option<WorkflowRunSnapshot> {
        self.runs.get(id).cloned()
    }

    pub(crate) fn upsert(&mut self, run: WorkflowRunSnapshot) -> Result<(), String> {
        let mut runs = self.runs.clone();
        runs.insert(run.id.clone(), run);
        save(&self.path, &runs)?;
        self.runs = runs;
        Ok(())
    }

    pub(crate) fn delete_for_project(
        &mut self,
        project_id: &ProjectId,
    ) -> Result<Vec<WorkflowRunId>, String> {
        let mut deleted = self
            .runs
            .values()
            .filter(|run| run.project_id.as_ref() == Some(project_id))
            .map(|run| run.id.clone())
            .collect::<Vec<_>>();
        if deleted.is_empty() {
            return Ok(deleted);
        }
        let mut runs = self.runs.clone();
        for id in &deleted {
            runs.remove(id);
        }
        save(&self.path, &runs)?;
        self.runs = runs;
        deleted.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(deleted)
    }

    fn save_current(&self) -> Result<(), String> {
        save(&self.path, &self.runs)
    }
}

fn resolve_override_path(override_path: Option<&str>) -> Option<PathBuf> {
    override_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn default_path_for_home(home_dir: &Path) -> PathBuf {
    home_dir.join(".tyde").join("workflow_runs.json")
}

fn read_from_disk(path: &Path) -> Result<HashMap<WorkflowRunId, WorkflowRunSnapshot>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let mut value: serde_json::Value = serde_json::from_str(&contents).map_err(|err| {
                format!(
                    "failed to parse workflow run store {}: {err}",
                    path.display()
                )
            })?;
            // A coordinator spec persists a typed `BackendKind`, so a run
            // recorded against the old `"acp"` name is normalized here rather
            // than being written back out in the stale spelling forever.
            crate::store::legacy_backend_kind::rewrite_legacy_acp_backend_kinds(&mut value);
            let store: StoreFile = serde_json::from_value(value).map_err(|err| {
                format!(
                    "failed to parse workflow run store {}: {err}",
                    path.display()
                )
            })?;
            Ok(store.runs)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => Err(format!(
            "failed to read workflow run store {}: {err}",
            path.display()
        )),
    }
}

fn save(path: &Path, runs: &HashMap<WorkflowRunId, WorkflowRunSnapshot>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create workflow run store dir {}: {err}",
                parent.display()
            )
        })?;
    }
    let store = StoreFile {
        version: STORE_VERSION,
        runs: runs.clone(),
    };
    let json = serde_json::to_string_pretty(&store)
        .map_err(|err| format!("failed to serialize workflow run store: {err}"))?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(|err| {
            format!(
                "failed to create workflow run store temp {}: {err}",
                tmp.display()
            )
        })?;
        file.write_all(json.as_bytes()).map_err(|err| {
            format!(
                "failed to write workflow run store temp {}: {err}",
                tmp.display()
            )
        })?;
        file.flush().map_err(|err| {
            format!(
                "failed to flush workflow run store temp {}: {err}",
                tmp.display()
            )
        })?;
    }
    std::fs::rename(&tmp, path).map_err(|err| {
        format!(
            "failed to replace workflow run store {}: {err}",
            path.display()
        )
    })
}

type WorkflowRunCall = Box<
    dyn FnOnce(&mut WorkflowRunStore, &tokio::sync::watch::Sender<Vec<WorkflowRunSnapshot>>) + Send,
>;

enum WorkflowRunCommand {
    Call(WorkflowRunCall),
    Stop(tokio::sync::oneshot::Sender<()>),
}

pub(crate) struct WorkflowRunHandle {
    calls: tokio::sync::mpsc::UnboundedSender<WorkflowRunCommand>,
    snapshot: tokio::sync::watch::Receiver<Vec<WorkflowRunSnapshot>>,
}

impl WorkflowRunHandle {
    pub(crate) fn spawn(mut store: WorkflowRunStore) -> Result<Self, String> {
        let (calls, mut inbox) = tokio::sync::mpsc::unbounded_channel::<WorkflowRunCommand>();
        let (snapshot, current) = tokio::sync::watch::channel(store.list());
        std::thread::Builder::new()
            .name("tyde-workflow-runs".into())
            .spawn(move || {
                while let Some(command) = inbox.blocking_recv() {
                    match command {
                        WorkflowRunCommand::Call(call) => call(&mut store, &snapshot),
                        WorkflowRunCommand::Stop(reply) => {
                            let _ = reply.send(());
                            return;
                        }
                    }
                }
            })
            .map_err(|error| format!("start workflow-run owner: {error}"))?;
        Ok(Self {
            calls,
            snapshot: current,
        })
    }

    pub(crate) fn list(&self) -> Vec<WorkflowRunSnapshot> {
        self.snapshot.borrow().clone()
    }

    pub(crate) fn get(&self, id: &WorkflowRunId) -> Option<WorkflowRunSnapshot> {
        self.snapshot
            .borrow()
            .iter()
            .find(|run| &run.id == id)
            .cloned()
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        let (reply, response) = tokio::sync::oneshot::channel();
        self.calls
            .send(WorkflowRunCommand::Stop(reply))
            .map_err(|_| "storage owner already stopped".to_owned())?;
        response
            .await
            .map_err(|_| "storage owner stopped before acknowledgement".to_owned())
    }

    pub(crate) async fn call<T, F>(&self, operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut WorkflowRunStore) -> Result<T, String> + Send + 'static,
    {
        let (reply, response) = tokio::sync::oneshot::channel();
        let queued = std::time::Instant::now();
        let span = tracing::Span::current();
        self.calls
            .send(WorkflowRunCommand::Call(Box::new(move |store, snapshot| {
                let scope = span.enter();
                let started = std::time::Instant::now();
                let result = operation(store);
                tracing::info!(target: "tyde::project_operation", stage = "workflow_runs", queue_ms = started.duration_since(queued).as_millis(), elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(), "serialized cleanup store operation completed");
                snapshot.send_replace(store.list());
                let _ = reply.send(result);
                drop(scope);
            })))
            .map_err(|_| "workflow-run owner stopped".to_owned())?;
        response
            .await
            .map_err(|_| "workflow-run owner stopped".to_owned())?
    }
}
