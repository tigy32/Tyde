use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{WorkflowRunId, WorkflowRunSnapshot, WorkflowRunSnapshotStatus};
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
        self.runs.insert(run.id.clone(), run);
        self.save_current()
    }

    fn save_current(&self) -> Result<(), String> {
        save(&self.path, &self.runs)
    }
}

fn read_from_disk(path: &Path) -> Result<HashMap<WorkflowRunId, WorkflowRunSnapshot>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let store: StoreFile = serde_json::from_str(&contents).map_err(|err| {
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

#[cfg(test)]
mod tests {
    use protocol::{
        BackendAccessMode, BackendKind, ProjectRootPath, WorkflowCoordinatorSpec, WorkflowId,
        WorkflowRunId, WorkflowRunSnapshot, WorkflowRunSnapshotStatus, WorkflowSource,
        WorkflowSourceScope,
    };

    use super::WorkflowRunStore;

    fn run(status: WorkflowRunSnapshotStatus) -> WorkflowRunSnapshot {
        WorkflowRunSnapshot {
            id: WorkflowRunId("run-1".to_owned()),
            workflow_id: WorkflowId("build".to_owned()),
            workflow_name: "Build".to_owned(),
            source: WorkflowSource {
                scope: WorkflowSourceScope::Project {
                    project_id: protocol::ProjectId("project-1".to_owned()),
                    root: ProjectRootPath("/repo".to_owned()),
                },
                path: "/repo/.tyde/workflows/build.md".to_owned(),
            },
            project_id: Some(protocol::ProjectId("project-1".to_owned())),
            coordinator_agent_id: None,
            coordinator: WorkflowCoordinatorSpec {
                backend: BackendKind::Codex,
                access_mode: BackendAccessMode::ReadOnly,
            },
            status,
            inputs: std::collections::HashMap::new(),
            steps: Vec::new(),
            agent_ids: Vec::new(),
            summary: None,
            error: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            completed_at_ms: None,
        }
    }

    #[test]
    fn load_marks_running_runs_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workflow_runs.json");
        let mut store = WorkflowRunStore::load(path.clone()).unwrap();
        store
            .upsert(run(WorkflowRunSnapshotStatus::Running))
            .unwrap();

        let reloaded = WorkflowRunStore::load(path).unwrap();
        let loaded = reloaded.get(&WorkflowRunId("run-1".to_owned())).unwrap();
        assert_eq!(loaded.status, WorkflowRunSnapshotStatus::Failed);
        assert!(
            loaded
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("restarted")
        );
        assert!(loaded.completed_at_ms.is_some());
    }
}
