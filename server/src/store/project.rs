use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{Project, ProjectId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, Project>,
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
        let mut records = Self::read_from_disk(&self.path)?;
        let project = Project {
            id: id.clone(),
            name,
            roots,
            sort_order: Self::next_sort_order(&records),
        };
        let previous = records.insert(id.0.clone(), project.clone());
        assert!(
            previous.is_none(),
            "project store generated duplicate project id {}",
            id
        );
        self.save(&records)?;
        Ok(project)
    }

    pub fn rename(&self, id: &ProjectId, name: String) -> Result<Project, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let Some(project) = records.get_mut(&id.0) else {
            return Err(format!("cannot rename missing project {}", id));
        };
        project.name = name;
        let updated = project.clone();
        self.save(&records)?;
        Ok(updated)
    }

    pub fn reorder(&self, project_ids: Vec<ProjectId>) -> Result<Vec<Project>, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let current_projects = Self::ordered_projects(&records);
        let mut seen_ids = HashSet::new();
        for project_id in &project_ids {
            if !records.contains_key(&project_id.0) {
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
            let Some(project) = records.get_mut(&project_id.0) else {
                return Err(format!("cannot reorder missing project {}", project_id));
            };
            project.sort_order = index as u64;
        }

        self.save(&records)?;
        Ok(Self::ordered_projects(&records))
    }

    pub fn add_root(&self, id: &ProjectId, root: String) -> Result<Project, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let Some(project) = records.get_mut(&id.0) else {
            return Err(format!("cannot add root to missing project {}", id));
        };
        if project.roots.iter().any(|existing| existing == &root) {
            return Err(format!("project {} already contains root {}", id, root));
        }
        project.roots.push(root);
        let updated = project.clone();
        self.save(&records)?;
        Ok(updated)
    }

    pub fn delete(&self, id: &ProjectId) -> Result<Project, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let Some(project) = records.remove(&id.0) else {
            return Err(format!("cannot delete missing project {}", id));
        };
        self.save(&records)?;
        Ok(project)
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, Project>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<StoreFile>(&contents)
                .map(|store| store.records)
                .map_err(|err| format!("Failed to parse project store {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read project store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, records: &HashMap<String, Project>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
        .map_err(|err| format!("Failed to serialize project store: {err}"))?;

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
