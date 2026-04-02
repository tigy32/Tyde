use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    #[serde(default)]
    pub backend_session_id: Option<String>,
    pub backend_kind: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub user_alias: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub workspace_root: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub message_count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, SessionRecord>,
}

#[derive(Debug)]
pub struct SessionStore {
    records: HashMap<String, SessionRecord>,
    path: PathBuf,
    dirty: bool,
}

impl SessionStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let records = match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<StoreFile>(&contents) {
                Ok(store_file) => store_file.records,
                Err(err) => {
                    // Back up the corrupt file so data isn't silently lost on next save
                    let corrupt_path = path.with_extension("json.corrupt");
                    tracing::error!(
                        "Session store at {} is corrupt ({err}), backing up to {}",
                        path.display(),
                        corrupt_path.display(),
                    );
                    if let Err(rename_err) = std::fs::rename(&path, &corrupt_path) {
                        tracing::error!("Failed to back up corrupt session store: {rename_err}");
                    }
                    return Err(format!(
                        "Failed to parse session store at {}: {err}",
                        path.display()
                    ));
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    "Session store not found at {}, starting empty",
                    path.display()
                );
                HashMap::new()
            }
            Err(err) => {
                return Err(format!(
                    "Failed to read session store at {}: {err}",
                    path.display()
                ));
            }
        };
        let mut store = Self {
            records,
            path,
            dirty: false,
        };
        match store.delete_orphaned() {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("Cleaned up {count} orphaned session records");
                }
            }
            Err(err) => {
                tracing::error!("Failed to clean up orphaned session records: {err}");
            }
        }
        Ok(store)
    }

    fn save(&mut self) -> Result<(), String> {
        let store_file = StoreFile {
            records: self.records.clone(),
        };
        let json = serde_json::to_string_pretty(&store_file)
            .map_err(|err| format!("Failed to serialize session store: {err}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("Failed to create session store directory: {err}"))?;
        }
        std::fs::write(&self.path, json).map_err(|err| {
            format!(
                "Failed to write session store to {}: {err}",
                self.path.display()
            )
        })?;
        self.dirty = false;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), String> {
        if self.dirty {
            self.save()
        } else {
            Ok(())
        }
    }

    pub fn get(&self, id: &str) -> Option<&SessionRecord> {
        self.records.get(id)
    }

    pub fn get_by_backend_session(
        &self,
        backend_kind: &str,
        backend_session_id: &str,
    ) -> Option<&SessionRecord> {
        self.records.values().find(|r| {
            r.backend_kind == backend_kind
                && r.backend_session_id.as_deref() == Some(backend_session_id)
        })
    }

    pub fn create(
        &mut self,
        backend_kind: &str,
        workspace_root: Option<&str>,
    ) -> Result<SessionRecord, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let record = SessionRecord {
            id: id.clone(),
            backend_session_id: None,
            backend_kind: backend_kind.to_string(),
            alias: None,
            user_alias: None,
            parent_id: None,
            workspace_root: workspace_root.map(|s| s.to_string()),
            created_at_ms: now,
            updated_at_ms: now,
            message_count: 0,
        };
        self.records.insert(id, record.clone());
        self.save()?;
        Ok(record)
    }

    pub fn set_backend_session_id(
        &mut self,
        id: &str,
        backend_session_id: &str,
    ) -> Result<(), String> {
        if let Some(record) = self.records.get_mut(id) {
            record.backend_session_id = Some(backend_session_id.to_string());
            record.updated_at_ms = now_ms();
            self.dirty = true;
        }
        Ok(())
    }

    pub fn set_alias(&mut self, id: &str, alias: &str) -> Result<(), String> {
        if let Some(record) = self.records.get_mut(id) {
            record.alias = Some(alias.to_string());
            record.updated_at_ms = now_ms();
            self.dirty = true;
        }
        Ok(())
    }

    pub fn set_user_alias(&mut self, id: &str, user_alias: &str) -> Result<(), String> {
        if let Some(record) = self.records.get_mut(id) {
            if user_alias.is_empty() {
                record.user_alias = None;
            } else {
                record.user_alias = Some(user_alias.to_string());
            }
            record.updated_at_ms = now_ms();
            self.save()?;
        }
        Ok(())
    }

    pub fn set_parent(&mut self, id: &str, parent_id: &str) -> Result<(), String> {
        if let Some(record) = self.records.get_mut(id) {
            record.parent_id = Some(parent_id.to_string());
            record.updated_at_ms = now_ms();
            self.dirty = true;
        }
        Ok(())
    }

    pub fn increment_message_count(&mut self, id: &str) -> Result<(), String> {
        if let Some(record) = self.records.get_mut(id) {
            record.message_count += 1;
            record.updated_at_ms = now_ms();
            self.dirty = true;
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionRecord> {
        self.records.values().cloned().collect()
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        if self.records.remove(id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    pub fn delete_orphaned(&mut self) -> Result<usize, String> {
        let cutoff = now_ms().saturating_sub(24 * 60 * 60 * 1000);
        let to_delete: Vec<String> = self
            .records
            .values()
            .filter(|r| {
                r.backend_session_id.is_none() && r.message_count == 0 && r.created_at_ms < cutoff
            })
            .map(|r| r.id.clone())
            .collect();
        let count = to_delete.len();
        for id in &to_delete {
            self.records.remove(id);
        }
        if count > 0 {
            self.save()?;
        }
        Ok(count)
    }
}
