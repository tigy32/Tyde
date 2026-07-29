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
        self.runs.insert(run.id.clone(), run);
        self.save_current()
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
        for id in &deleted {
            self.runs.remove(id);
        }
        self.save_current()?;
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
            // recorded against the old `"kiro"` name would fail the whole file
            // and take every other run's history with it.
            crate::store::legacy_backend_kind::rewrite_legacy_kiro_backend_kinds(&mut value);
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use protocol::{
        BackendAccessMode, BackendKind, ProjectRootPath, WorkflowCoordinatorSpec, WorkflowId,
        WorkflowRunId, WorkflowRunSnapshot, WorkflowRunSnapshotStatus, WorkflowSource,
        WorkflowSourceScope,
    };

    use super::{WorkflowRunStore, default_path_for_home, resolve_override_path};

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
    fn legacy_kiro_coordinator_migrates_instead_of_failing_the_whole_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workflow_runs.json");
        // Seed a real run, then rewrite its coordinator back to the legacy
        // spelling — the shape an install that predates the rename has on disk.
        let mut store = WorkflowRunStore::load(path.clone()).unwrap();
        store
            .upsert(run(WorkflowRunSnapshotStatus::Completed))
            .unwrap();
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["runs"]["run-1"]["coordinator"]["backend"] = serde_json::json!("kiro");
        std::fs::write(&path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

        let reloaded = WorkflowRunStore::load(path)
            .expect("a run recorded under the old backend name must not fail the whole run store");
        let loaded = reloaded.get(&WorkflowRunId("run-1".to_owned())).unwrap();
        assert_eq!(loaded.coordinator.backend, BackendKind::Acp);
        assert_eq!(
            loaded.coordinator.access_mode,
            BackendAccessMode::ReadOnly,
            "the rest of the coordinator spec must survive the rename"
        );
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

    #[test]
    fn delete_for_project_removes_only_matching_runs_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workflow_runs.json");
        let mut store = WorkflowRunStore::load(path.clone()).unwrap();
        let target_project = protocol::ProjectId("project-1".to_owned());
        let mut kept = run(WorkflowRunSnapshotStatus::Completed);
        kept.id = WorkflowRunId("run-2".to_owned());
        kept.project_id = Some(protocol::ProjectId("project-2".to_owned()));
        store
            .upsert(run(WorkflowRunSnapshotStatus::Completed))
            .unwrap();
        store.upsert(kept.clone()).unwrap();

        assert_eq!(
            store.delete_for_project(&target_project).unwrap(),
            vec![WorkflowRunId("run-1".to_owned())]
        );
        assert!(store.get(&WorkflowRunId("run-1".to_owned())).is_none());
        assert_eq!(store.get(&kept.id), Some(kept.clone()));
        assert!(
            store
                .delete_for_project(&target_project)
                .unwrap()
                .is_empty()
        );

        let reloaded = WorkflowRunStore::load(path).unwrap();
        assert!(reloaded.get(&WorkflowRunId("run-1".to_owned())).is_none());
        assert_eq!(reloaded.get(&kept.id), Some(kept));
    }

    #[test]
    fn workflow_run_store_path_honors_override() {
        assert_eq!(
            resolve_override_path(Some(" /isolated/workflow_runs.json ")),
            Some(PathBuf::from("/isolated/workflow_runs.json"))
        );
        assert_eq!(resolve_override_path(Some(" ")), None);
        assert_eq!(resolve_override_path(None), None);
        assert_eq!(
            default_path_for_home(Path::new("/home/user")),
            PathBuf::from("/home/user/.tyde/workflow_runs.json")
        );
    }
}
