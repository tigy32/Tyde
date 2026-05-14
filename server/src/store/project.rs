use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{Project, ProjectId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    records: HashMap<String, StoredProject>,
}

impl StoreFile {
    fn empty() -> Self {
        Self {
            version: None,
            records: HashMap::new(),
        }
    }

    fn projects(&self) -> HashMap<String, Project> {
        self.records
            .iter()
            .map(|(id, record)| (id.clone(), record.to_project()))
            .collect()
    }

    fn uses_source_shape(&self) -> bool {
        self.version == Some(2)
            || self
                .records
                .values()
                .any(|record| matches!(record, StoredProject::Source(_)))
    }

    fn normalize_version_for_save(&mut self) {
        if self.uses_source_shape() {
            self.version = Some(2);
            for record in self.records.values_mut() {
                if let StoredProject::Legacy(project) = record {
                    *record = StoredProject::Source(SourceProject {
                        id: project.id.clone(),
                        name: project.name.clone(),
                        sort_order: project.sort_order,
                        source: ProjectSource::Standalone {
                            roots: project.roots.clone(),
                        },
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredProject {
    Legacy(LegacyProject),
    Source(SourceProject),
}

impl StoredProject {
    fn from_project(project: Project, use_source_shape: bool) -> Self {
        if use_source_shape {
            StoredProject::Source(SourceProject {
                id: project.id,
                name: project.name,
                sort_order: project.sort_order,
                source: ProjectSource::Standalone {
                    roots: project.roots,
                },
            })
        } else {
            StoredProject::Legacy(LegacyProject {
                id: project.id,
                name: project.name,
                roots: project.roots,
                sort_order: project.sort_order,
            })
        }
    }

    fn to_project(&self) -> Project {
        match self {
            StoredProject::Legacy(project) => Project {
                id: project.id.clone(),
                name: project.name.clone(),
                roots: project.roots.clone(),
                sort_order: project.sort_order,
            },
            StoredProject::Source(project) => Project {
                id: project.id.clone(),
                name: project.name.clone(),
                roots: project.source.root_paths(),
                sort_order: project.sort_order,
            },
        }
    }

    fn set_name(&mut self, name: String) {
        match self {
            StoredProject::Legacy(project) => project.name = name,
            StoredProject::Source(project) => project.name = name,
        }
    }

    fn set_sort_order(&mut self, sort_order: u64) {
        match self {
            StoredProject::Legacy(project) => project.sort_order = sort_order,
            StoredProject::Source(project) => project.sort_order = sort_order,
        }
    }

    fn standalone_roots_mut(&mut self) -> Option<&mut Vec<String>> {
        match self {
            StoredProject::Legacy(project) => Some(&mut project.roots),
            StoredProject::Source(project) => project.source.standalone_roots_mut(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyProject {
    id: ProjectId,
    name: String,
    roots: Vec<String>,
    #[serde(default)]
    sort_order: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceProject {
    id: ProjectId,
    name: String,
    #[serde(default)]
    sort_order: u64,
    source: ProjectSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProjectSource {
    Standalone {
        roots: Vec<String>,
    },
    GitWorkbench {
        parent_project_id: ProjectId,
        branch: String,
        roots: Vec<WorkbenchRoot>,
    },
}

impl ProjectSource {
    fn root_paths(&self) -> Vec<String> {
        match self {
            ProjectSource::Standalone { roots } => roots.clone(),
            ProjectSource::GitWorkbench { roots, .. } => roots
                .iter()
                .map(|root| root.worktree_root.clone())
                .collect(),
        }
    }

    fn standalone_roots_mut(&mut self) -> Option<&mut Vec<String>> {
        match self {
            ProjectSource::Standalone { roots } => Some(roots),
            ProjectSource::GitWorkbench { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkbenchRoot {
    parent_root: String,
    worktree_root: String,
}

#[derive(Debug)]
pub struct ProjectStore {
    path: PathBuf,
}

impl ProjectStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let _ = Self::read_from_disk(&path)?;
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_PROJECT_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("projects.json"))
    }

    pub fn list(&self) -> Result<Vec<Project>, String> {
        let records = Self::read_from_disk(&self.path)?;
        Ok(Self::ordered_projects(&records))
    }

    pub fn get(&self, id: &ProjectId) -> Option<Project> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn create(&self, name: String, roots: Vec<String>) -> Result<Project, String> {
        let id = ProjectId(Uuid::new_v4().to_string());
        let mut store = Self::read_store_from_disk(&self.path)?;
        let records = store.projects();
        let project = Project {
            id: id.clone(),
            name,
            roots,
            sort_order: Self::next_sort_order(&records),
        };
        let previous = store.records.insert(
            id.0.clone(),
            StoredProject::from_project(project.clone(), store.uses_source_shape()),
        );
        assert!(
            previous.is_none(),
            "project store generated duplicate project id {}",
            id
        );
        self.save_store(&mut store)?;
        Ok(project)
    }

    pub fn rename(&self, id: &ProjectId, name: String) -> Result<Project, String> {
        let mut store = Self::read_store_from_disk(&self.path)?;
        let Some(record) = store.records.get_mut(&id.0) else {
            return Err(format!("cannot rename missing project {}", id));
        };
        record.set_name(name);
        let updated = record.to_project();
        self.save_store(&mut store)?;
        Ok(updated)
    }

    pub fn reorder(&self, project_ids: Vec<ProjectId>) -> Result<Vec<Project>, String> {
        let mut store = Self::read_store_from_disk(&self.path)?;
        let records = store.projects();
        let current_projects = Self::ordered_projects(&records);
        let mut seen_ids = HashSet::new();
        for project_id in &project_ids {
            if !store.records.contains_key(&project_id.0) {
                return Err(format!("cannot reorder missing project {}", project_id));
            }
            if !seen_ids.insert(project_id.0.clone()) {
                return Err(format!(
                    "project reorder contains duplicate id {}",
                    project_id
                ));
            }
        }

        let mut ordered_ids = project_ids;
        ordered_ids.extend(
            current_projects
                .iter()
                .filter(|project| !seen_ids.contains(&project.id.0))
                .map(|project| project.id.clone()),
        );

        for (index, project_id) in ordered_ids.into_iter().enumerate() {
            let Some(project) = store.records.get_mut(&project_id.0) else {
                return Err(format!("cannot reorder missing project {}", project_id));
            };
            project.set_sort_order(index as u64);
        }

        self.save_store(&mut store)?;
        Ok(Self::ordered_projects(&store.projects()))
    }

    pub fn add_root(&self, id: &ProjectId, root: String) -> Result<Project, String> {
        let mut store = Self::read_store_from_disk(&self.path)?;
        let Some(project) = store.records.get_mut(&id.0) else {
            return Err(format!("cannot add root to missing project {}", id));
        };
        let Some(roots) = project.standalone_roots_mut() else {
            return Err(format!("cannot add root to workbench project {}", id));
        };
        if roots.iter().any(|existing| existing == &root) {
            return Err(format!("project {} already contains root {}", id, root));
        }
        roots.push(root);
        let updated = project.to_project();
        self.save_store(&mut store)?;
        Ok(updated)
    }

    pub fn delete_root(&self, id: &ProjectId, root: &str) -> Result<Project, String> {
        let mut store = Self::read_store_from_disk(&self.path)?;
        let Some(project) = store.records.get_mut(&id.0) else {
            return Err(format!("cannot delete root from missing project {}", id));
        };
        let Some(roots) = project.standalone_roots_mut() else {
            return Err(format!("cannot delete root from workbench project {}", id));
        };
        let original_len = roots.len();
        roots.retain(|existing| existing != root);
        if roots.len() == original_len {
            return Err(format!("project {} does not contain root {}", id, root));
        }
        let updated = project.to_project();
        self.save_store(&mut store)?;
        Ok(updated)
    }

    pub fn delete(&self, id: &ProjectId) -> Result<Project, String> {
        let mut store = Self::read_store_from_disk(&self.path)?;
        let Some(project) = store.records.remove(&id.0) else {
            return Err(format!("cannot delete missing project {}", id));
        };
        let project = project.to_project();
        self.save_store(&mut store)?;
        Ok(project)
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, Project>, String> {
        Ok(Self::read_store_from_disk(path)?.projects())
    }

    fn read_store_from_disk(path: &Path) -> Result<StoreFile, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<StoreFile>(&contents)
                .map_err(|err| format!("Failed to parse project store {}: {err}", path.display()))
                .and_then(|store| {
                    Self::validate_store_version(&store, path)?;
                    Ok(store)
                }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::empty()),
            Err(err) => Err(format!(
                "Failed to read project store {}: {err}",
                path.display()
            )),
        }
    }

    fn validate_store_version(store: &StoreFile, path: &Path) -> Result<(), String> {
        match store.version {
            None | Some(2) => Ok(()),
            Some(version) => Err(format!(
                "Failed to parse project store {}: unsupported version {}",
                path.display(),
                version
            )),
        }
    }

    fn save_store(&self, store: &mut StoreFile) -> Result<(), String> {
        store.normalize_version_for_save();
        let json = serde_json::to_string_pretty(store)
            .map_err(|err| format!("Failed to serialize project store: {err}"))?;

        self.write_json(&json)
    }

    fn write_json(&self, json: &str) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("Project store path has no parent: {}", self.path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create project store directory: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp project store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp project store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp project store file: {err}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "Failed to atomically replace project store {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    fn ordered_projects(records: &HashMap<String, Project>) -> Vec<Project> {
        let mut projects: Vec<_> = records.values().cloned().collect();
        projects.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then(left.name.cmp(&right.name))
                .then(left.id.0.cmp(&right.id.0))
        });
        projects
    }

    fn next_sort_order(records: &HashMap<String, Project>) -> u64 {
        records
            .values()
            .map(|project| project.sort_order)
            .max()
            .map(|max_order| max_order.saturating_add(1))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn write_v2_store(path: &Path) {
        std::fs::write(
            path,
            r#"{
  "version": 2,
  "records": {
    "standalone": {
      "id": "standalone",
      "name": "Standalone",
      "sort_order": 0,
      "source": {
        "kind": "standalone",
        "roots": ["/repo"]
      }
    },
    "workbench": {
      "id": "workbench",
      "name": "Feature",
      "sort_order": 1,
      "source": {
        "kind": "git_workbench",
        "parent_project_id": "standalone",
        "branch": "feature",
        "roots": [
          {
            "parent_root": "/repo",
            "worktree_root": "/repo--feature"
          }
        ]
      }
    }
  }
}"#,
        )
        .expect("write v2 project store");
    }

    #[test]
    fn loads_v2_source_records_as_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        write_v2_store(&path);

        let store = ProjectStore::load(path).expect("load v2 project store");
        let projects = store.list().expect("list projects");

        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id.0, "standalone");
        assert_eq!(projects[0].roots, vec!["/repo".to_owned()]);
        assert_eq!(projects[1].id.0, "workbench");
        assert_eq!(projects[1].roots, vec!["/repo--feature".to_owned()]);
    }

    #[test]
    fn rename_preserves_v2_source_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        write_v2_store(&path);

        let store = ProjectStore::load(path.clone()).expect("load v2 project store");
        let renamed = store
            .rename(&ProjectId("workbench".to_owned()), "Renamed".to_owned())
            .expect("rename workbench");

        assert_eq!(renamed.name, "Renamed");
        assert_eq!(renamed.roots, vec!["/repo--feature".to_owned()]);

        let contents = std::fs::read_to_string(path).expect("read saved project store");
        let value: Value = serde_json::from_str(&contents).expect("parse saved project store");
        let record = &value["records"]["workbench"];

        assert_eq!(value["version"], 2);
        assert_eq!(record["name"], "Renamed");
        assert!(record.get("roots").is_none());
        assert_eq!(record["source"]["kind"], "git_workbench");
        assert_eq!(
            record["source"]["roots"][0]["worktree_root"],
            "/repo--feature"
        );
    }

    #[test]
    fn add_root_preserves_v2_standalone_source_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        write_v2_store(&path);

        let store = ProjectStore::load(path.clone()).expect("load v2 project store");
        let updated = store
            .add_root(&ProjectId("standalone".to_owned()), "/repo2".to_owned())
            .expect("add root");

        assert_eq!(updated.roots, vec!["/repo".to_owned(), "/repo2".to_owned()]);

        let contents = std::fs::read_to_string(path).expect("read saved project store");
        let value: Value = serde_json::from_str(&contents).expect("parse saved project store");
        let record = &value["records"]["standalone"];

        assert!(record.get("roots").is_none());
        assert_eq!(record["source"]["kind"], "standalone");
        assert_eq!(record["source"]["roots"][0], "/repo");
        assert_eq!(record["source"]["roots"][1], "/repo2");
    }
}
