use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{ProjectId, Steering, SteeringId, SteeringScope};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, Steering>,
    #[serde(default)]
    initialized_defaults: HashSet<String>,
}

#[derive(Debug)]
pub struct SteeringStore {
    path: PathBuf,
}

impl SteeringStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let mut file = Self::read_from_disk(&path)?;
        let store = Self { path };
        let default = atomic_rule_guidance();
        // Remember initialization even after deletion; absence is a user choice.
        if file.initialized_defaults.insert(default.id.0.clone()) {
            file.records.entry(default.id.0.clone()).or_insert(default);
            store.save(&file)?;
        }
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_STEERING_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?
            .join(".tyde")
            .join("steering.json"))
    }

    pub fn list(&self) -> Result<Vec<Steering>, String> {
        let mut records = Self::read_from_disk(&self.path)?
            .records
            .into_values()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.title
                .cmp(&right.title)
                .then(left.id.0.cmp(&right.id.0))
        });
        Ok(records)
    }

    pub fn get(&self, id: &SteeringId) -> Option<Steering> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|file| file.records.get(&id.0).cloned())
    }

    pub fn upsert(&self, steering: Steering) -> Result<Steering, String> {
        validate_steering(&steering)?;
        let mut file = Self::read_from_disk(&self.path)?;
        file.records.insert(steering.id.0.clone(), steering.clone());
        self.save(&file)?;
        Ok(steering)
    }

    pub fn delete(&self, id: &SteeringId) -> Result<SteeringId, String> {
        let mut file = Self::read_from_disk(&self.path)?;
        if file.records.remove(&id.0).is_none() {
            return Err(format!("cannot delete missing steering {}", id));
        }
        self.save(&file)?;
        Ok(id.clone())
    }

    pub fn delete_for_project(&self, project_id: &ProjectId) -> Result<Vec<SteeringId>, String> {
        let mut file = Self::read_from_disk(&self.path)?;
        let mut deleted = file
            .records
            .values()
            .filter(|steering| {
                matches!(&steering.scope, SteeringScope::Project(id) if id == project_id)
            })
            .map(|steering| steering.id.clone())
            .collect::<Vec<_>>();
        if deleted.is_empty() {
            return Ok(deleted);
        }
        deleted.sort_by(|left, right| left.0.cmp(&right.0));
        for id in &deleted {
            file.records.remove(&id.0);
        }
        self.save(&file)?;
        Ok(deleted)
    }

    fn read_from_disk(path: &Path) -> Result<StoreFile, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let file = serde_json::from_str::<StoreFile>(&contents).map_err(|err| {
                    format!("Failed to parse steering store {}: {err}", path.display())
                })?;
                for steering in file.records.values() {
                    validate_steering(steering).map_err(|err| {
                        format!("Invalid steering store {}: {err}", path.display())
                    })?;
                }
                Ok(file)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile {
                records: HashMap::new(),
                initialized_defaults: HashSet::new(),
            }),
            Err(err) => Err(format!(
                "Failed to read steering store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, file: &StoreFile) -> Result<(), String> {
        let json = serde_json::to_string_pretty(file)
            .map_err(|err| format!("Failed to serialize steering store: {err}"))?;

        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("Steering store path has no parent: {}", self.path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create steering store directory: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp steering store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp steering store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp steering store file: {err}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "Failed to atomically replace steering store {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

fn validate_steering(steering: &Steering) -> Result<(), String> {
    if steering.id.0.trim().is_empty() {
        return Err("steering id must not be empty".to_string());
    }
    if steering.title.trim().is_empty() {
        return Err(format!("steering {} title must not be empty", steering.id));
    }
    if steering.content.trim().is_empty() {
        return Err(format!(
            "steering {} content must not be empty",
            steering.id
        ));
    }
    Ok(())
}

fn atomic_rule_guidance() -> Steering {
    Steering {
        id: SteeringId("tyde-atomic-rule-guidance".into()),
        scope: SteeringScope::Host,
        title: "Maintain atomic steering and review aspects".into(),
        content: "Keep each steering entry and review aspect concise and atomic: roughly tweet-sized is a writing guideline, not a limit. Before adding, inspect applicable entries for semantic overlap; edit or merge the existing rule and refine that same entry. Keep a prohibition and its approved exceptions together as one policy. Give genuinely independent feedback separate maintainable entries.".into(),
    }
}
