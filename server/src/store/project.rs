use std::collections::HashMap;
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
        let mut projects: Vec<_> = records.values().cloned().collect();
        projects.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(projects)
    }

    pub fn get(&self, id: &ProjectId) -> Option<Project> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn create(&self, name: String, roots: Vec<String>) -> Result<Project, String> {
        let id = ProjectId(Uuid::new_v4().to_string());
        let project = Project {
            id: id.clone(),
            name,
            roots,
        };
        let mut records = Self::read_from_disk(&self.path)?;
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
}
