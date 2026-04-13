use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub workspace_path: String,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub parent_project_id: Option<String>,
    #[serde(default)]
    pub workbench_kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, ProjectRecord>,
}

#[derive(Debug)]
pub struct ProjectStore {
    records: HashMap<String, ProjectRecord>,
    path: PathBuf,
}

impl ProjectStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let records = match Self::read_from_disk(&path) {
            Ok(r) => r,
            Err(err) => {
                tracing::error!("Failed to load project store: {err}. Starting with empty store.");
                HashMap::new()
            }
        };
        Ok(Self { records, path })
    }

    fn read_from_disk(path: &PathBuf) -> Result<HashMap<String, ProjectRecord>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str::<StoreFile>(&contents) {
                Ok(store_file) => Ok(store_file.records),
                Err(err) => {
                    let corrupt_path = path.with_extension("json.corrupt");
                    tracing::error!(
                        "Project store at {} is corrupt ({err}), backing up to {}",
                        path.display(),
                        corrupt_path.display(),
                    );
                    if let Err(rename_err) = std::fs::rename(path, &corrupt_path) {
                        tracing::error!("Failed to back up corrupt project store: {rename_err}");
                    }
                    Err(format!(
                        "Failed to parse project store at {}: {err}",
                        path.display()
                    ))
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read project store at {}: {err}",
                path.display()
            )),
        }
    }

    fn read_modify_write<F>(&mut self, modify: F) -> Result<(), String>
    where
        F: FnOnce(&mut HashMap<String, ProjectRecord>),
    {
        self.records = Self::read_from_disk(&self.path)?;
        modify(&mut self.records);
        self.save()
    }

    fn save(&mut self) -> Result<(), String> {
        let store_file = StoreFile {
            records: self.records.clone(),
        };
        let json = serde_json::to_string_pretty(&store_file)
            .map_err(|err| format!("Failed to serialize project store: {err}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("Failed to create project store directory: {err}"))?;
        }
        std::fs::write(&self.path, json).map_err(|err| {
            format!(
                "Failed to write project store to {}: {err}",
                self.path.display()
            )
        })
    }

    pub fn list(&mut self) -> Result<Vec<ProjectRecord>, String> {
        self.records = Self::read_from_disk(&self.path)?;
        // Deterministic order: parents (sorted by name) then their children (sorted by name)
        let mut parents: Vec<&ProjectRecord> = self
            .records
            .values()
            .filter(|r| r.parent_project_id.is_none())
            .collect();
        parents.sort_by(|a, b| a.name.cmp(&b.name));
        let mut result = Vec::with_capacity(self.records.len());
        for parent in parents {
            result.push(parent.clone());
            let mut children: Vec<&ProjectRecord> = self
                .records
                .values()
                .filter(|r| r.parent_project_id.as_deref() == Some(&parent.id))
                .collect();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            for child in children {
                result.push(child.clone());
            }
        }
        Ok(result)
    }

    #[allow(dead_code)]
    pub fn get(&mut self, id: &str) -> Option<&ProjectRecord> {
        if let Ok(fresh) = Self::read_from_disk(&self.path) {
            self.records = fresh;
        }
        self.records.get(id)
    }

    pub fn get_by_workspace_path(&mut self, path: &str) -> Option<&ProjectRecord> {
        if let Ok(fresh) = Self::read_from_disk(&self.path) {
            self.records = fresh;
        }
        self.records.values().find(|r| r.workspace_path == path)
    }

    pub fn add(&mut self, workspace_path: &str, name: &str) -> Result<ProjectRecord, String> {
        // Check for duplicate workspace_path
        if let Ok(fresh) = Self::read_from_disk(&self.path) {
            self.records = fresh;
        }
        if let Some(existing) = self
            .records
            .values()
            .find(|r| r.workspace_path == workspace_path)
        {
            return Ok(existing.clone());
        }

        let id = uuid::Uuid::new_v4().to_string();
        let record = ProjectRecord {
            id: id.clone(),
            name: name.to_string(),
            workspace_path: workspace_path.to_string(),
            roots: Vec::new(),
            parent_project_id: None,
            workbench_kind: None,
        };
        let record_clone = record.clone();
        self.read_modify_write(|records| {
            records.insert(id, record);
        })?;
        Ok(record_clone)
    }

    pub fn add_workbench(
        &mut self,
        parent_project_id: &str,
        workspace_path: &str,
        name: &str,
        kind: &str,
    ) -> Result<ProjectRecord, String> {
        // Check for duplicate workspace_path
        if let Ok(fresh) = Self::read_from_disk(&self.path) {
            self.records = fresh;
        }
        if let Some(existing) = self
            .records
            .values()
            .find(|r| r.workspace_path == workspace_path)
        {
            return Ok(existing.clone());
        }

        // Copy roots from parent — parent must exist
        let parent = self.records.get(parent_project_id).ok_or_else(|| {
            format!("Parent project '{parent_project_id}' not found in project store")
        })?;
        let parent_roots = parent.roots.clone();

        let id = uuid::Uuid::new_v4().to_string();
        let record = ProjectRecord {
            id: id.clone(),
            name: name.to_string(),
            workspace_path: workspace_path.to_string(),
            roots: parent_roots,
            parent_project_id: Some(parent_project_id.to_string()),
            workbench_kind: Some(kind.to_string()),
        };
        let record_clone = record.clone();
        self.read_modify_write(|records| {
            records.insert(id, record);
        })?;
        Ok(record_clone)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let id = id.to_string();
        // Also remove child workbenches
        self.read_modify_write(|records| {
            let child_ids: Vec<String> = records
                .values()
                .filter(|r| r.parent_project_id.as_deref() == Some(&id))
                .map(|r| r.id.clone())
                .collect();
            records.remove(&id);
            for child_id in child_ids {
                records.remove(&child_id);
            }
        })
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<(), String> {
        let id = id.to_string();
        let name = name.to_string();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                record.name = name;
            }
        })
    }

    pub fn update_roots(&mut self, id: &str, roots: Vec<String>) -> Result<(), String> {
        let id = id.to_string();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                record.roots = roots;
            }
        })
    }
}
