use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
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
}

impl SessionStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let records = Self::read_from_disk(&path)?;
        let mut store = Self { records, path };
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

    fn read_from_disk(path: &PathBuf) -> Result<HashMap<String, SessionRecord>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str::<StoreFile>(&contents) {
                Ok(store_file) => Ok(store_file.records),
                Err(err) => {
                    let corrupt_path = path.with_extension("json.corrupt");
                    tracing::error!(
                        "Session store at {} is corrupt ({err}), backing up to {}",
                        path.display(),
                        corrupt_path.display(),
                    );
                    if let Err(rename_err) = std::fs::rename(path, &corrupt_path) {
                        tracing::error!("Failed to back up corrupt session store: {rename_err}");
                    }
                    Err(format!(
                        "Failed to parse session store at {}: {err}",
                        path.display()
                    ))
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read session store at {}: {err}",
                path.display()
            )),
        }
    }

    /// Re-read the file from disk, apply a mutation, and write back.
    /// Minimizes the race window when multiple Tyde instances share the file.
    fn read_modify_write<F>(&mut self, modify: F) -> Result<(), String>
    where
        F: FnOnce(&mut HashMap<String, SessionRecord>),
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
            .map_err(|err| format!("Failed to serialize session store: {err}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("Failed to create session store directory: {err}"))?;
        }
        // Atomic write: write to a temp file in the same directory, then persist.
        // This avoids the O_TRUNC window where std::fs::write leaves the file
        // empty/partial, which can cause concurrent readers to see corrupt data
        // and trigger the .corrupt rename — wiping the store.
        // tempfile::NamedTempFile::persist uses rename(2) on Unix and ReplaceFile
        // on Windows, so the swap is atomic on both platforms.
        let dir = self.path.parent().ok_or("Session store path has no parent")?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|err| {
            format!("Failed to create temp file in {}: {err}", dir.display())
        })?;
        tmp.write_all(json.as_bytes()).map_err(|err| {
            format!("Failed to write session store temp file: {err}")
        })?;
        tmp.persist(&self.path).map_err(|err| {
            format!(
                "Failed to persist session store to {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    pub fn get(&mut self, id: &str) -> Option<&SessionRecord> {
        if let Ok(fresh) = Self::read_from_disk(&self.path) {
            self.records = fresh;
        }
        self.records.get(id)
    }

    pub fn get_by_backend_session(
        &mut self,
        backend_kind: &str,
        backend_session_id: &str,
    ) -> Option<&SessionRecord> {
        if let Ok(fresh) = Self::read_from_disk(&self.path) {
            self.records = fresh;
        }
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
        let record_clone = record.clone();
        self.read_modify_write(|records| {
            records.insert(id, record);
        })?;
        Ok(record_clone)
    }

    pub fn set_backend_session_id(
        &mut self,
        id: &str,
        backend_session_id: &str,
    ) -> Result<(), String> {
        // Enforce uniqueness: (backend_kind, backend_session_id) must not
        // already belong to a different record.
        self.records = Self::read_from_disk(&self.path)?;
        if let Some(record) = self.records.get(id) {
            let dominated = self.records.values().any(|r| {
                r.id != id
                    && r.backend_kind == record.backend_kind
                    && r.backend_session_id.as_deref() == Some(backend_session_id)
            });
            if dominated {
                return Err(format!(
                    "Duplicate backend session: another record already owns \
                     ({}, {})",
                    record.backend_kind, backend_session_id,
                ));
            }
        }
        let id = id.to_string();
        let backend_session_id = backend_session_id.to_string();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                record.backend_session_id = Some(backend_session_id);
                record.updated_at_ms = now_ms();
            }
        })
    }

    pub fn set_alias(&mut self, id: &str, alias: &str) -> Result<(), String> {
        let id = id.to_string();
        let alias = alias.to_string();
        let id_check = id.clone();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                record.alias = Some(alias);
                record.updated_at_ms = now_ms();
            }
        })?;
        if !self.records.contains_key(&id_check) {
            return Err(format!("Session record '{id_check}' not found"));
        }
        Ok(())
    }

    pub fn set_user_alias(&mut self, id: &str, user_alias: &str) -> Result<(), String> {
        let id = id.to_string();
        let user_alias = user_alias.to_string();
        let id_check = id.clone();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                if user_alias.is_empty() {
                    record.user_alias = None;
                } else {
                    record.user_alias = Some(user_alias);
                }
                record.updated_at_ms = now_ms();
            }
        })?;
        if !self.records.contains_key(&id_check) {
            return Err(format!("Session record '{id_check}' not found"));
        }
        Ok(())
    }

    pub fn set_parent(&mut self, id: &str, parent_id: &str) -> Result<(), String> {
        let id = id.to_string();
        let parent_id = parent_id.to_string();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                record.parent_id = Some(parent_id);
                record.updated_at_ms = now_ms();
            }
        })
    }

    pub fn increment_message_count(&mut self, id: &str) -> Result<(), String> {
        let id = id.to_string();
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&id) {
                record.message_count += 1;
                record.updated_at_ms = now_ms();
            }
        })
    }

    pub fn list(&mut self) -> Result<Vec<SessionRecord>, String> {
        self.records = Self::read_from_disk(&self.path)?;
        Ok(self.records.values().cloned().collect())
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        let id = id.to_string();
        self.read_modify_write(|records| {
            records.remove(&id);
        })
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
        if count > 0 {
            self.read_modify_write(|records| {
                for id in &to_delete {
                    records.remove(id);
                }
            })?;
        }
        Ok(count)
    }
}
