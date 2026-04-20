use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{Steering, SteeringId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, Steering>,
}

#[derive(Debug)]
pub struct SteeringStore {
    path: PathBuf,
}

impl SteeringStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let _ = Self::read_from_disk(&path)?;
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_STEERING_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("steering.json"))
    }

    pub fn list(&self) -> Result<Vec<Steering>, String> {
        let mut records = Self::read_from_disk(&self.path)?
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
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn upsert(&self, steering: Steering) -> Result<Steering, String> {
        validate_steering(&steering)?;
        let mut records = Self::read_from_disk(&self.path)?;
        records.insert(steering.id.0.clone(), steering.clone());
        self.save(&records)?;
        Ok(steering)
    }

    pub fn delete(&self, id: &SteeringId) -> Result<SteeringId, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        if records.remove(&id.0).is_none() {
            return Err(format!("cannot delete missing steering {}", id));
        }
        self.save(&records)?;
        Ok(id.clone())
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, Steering>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let records = serde_json::from_str::<StoreFile>(&contents)
                    .map(|store| store.records)
                    .map_err(|err| {
                        format!("Failed to parse steering store {}: {err}", path.display())
                    })?;
                for steering in records.values() {
                    validate_steering(steering).map_err(|err| {
                        format!("Invalid steering store {}: {err}", path.display())
                    })?;
                }
                Ok(records)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read steering store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, records: &HashMap<String, Steering>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
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
