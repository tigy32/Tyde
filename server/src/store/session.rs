use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    BackendKind, CustomAgentId, ProjectId, SessionId, SessionSettingsValues, SessionSummary,
};
use serde::{Deserialize, Serialize};

use crate::backend::BackendSession;

fn default_resumable() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub custom_agent_id: Option<CustomAgentId>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub user_alias: Option<String>,
    #[serde(default)]
    pub parent_id: Option<SessionId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub message_count: u32,
    #[serde(default)]
    pub token_count: Option<u64>,
    #[serde(default)]
    pub session_settings: Option<SessionSettingsValues>,
    #[serde(default = "default_resumable")]
    pub resumable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, SessionRecord>,
}

#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let _ = Self::read_from_disk(&path)?;
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SESSION_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("sessions.json"))
    }

    pub fn list(&self) -> Result<Vec<SessionRecord>, String> {
        let records = Self::read_from_disk(&self.path)?;
        let mut out: Vec<_> = records.into_values().collect();
        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(out)
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionRecord> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn upsert_backend_session(
        &self,
        session: &BackendSession,
        parent_id: Option<SessionId>,
        project_id: Option<ProjectId>,
        custom_agent_id: Option<CustomAgentId>,
    ) -> Result<SessionRecord, String> {
        let now = now_ms();
        self.read_modify_write(|records| {
            let entry = records
                .entry(session.id.0.clone())
                .or_insert_with(|| SessionRecord {
                    id: session.id.clone(),
                    backend_kind: session.backend_kind,
                    workspace_roots: session.workspace_roots.clone(),
                    project_id: project_id.clone(),
                    custom_agent_id: custom_agent_id.clone(),
                    alias: session.title.clone(),
                    user_alias: None,
                    parent_id: parent_id.clone(),
                    created_at_ms: session.created_at_ms.unwrap_or(now),
                    updated_at_ms: session.updated_at_ms.unwrap_or(now),
                    message_count: 0,
                    token_count: session.token_count,
                    session_settings: None,
                    resumable: session.resumable,
                });

            entry.backend_kind = session.backend_kind;
            entry.workspace_roots = session.workspace_roots.clone();
            entry.project_id = project_id;
            entry.custom_agent_id = custom_agent_id;
            if entry.alias.is_none() {
                entry.alias = session.title.clone();
            }
            if entry.parent_id.is_none() {
                entry.parent_id = parent_id;
            }
            if let Some(created) = session.created_at_ms {
                entry.created_at_ms = created;
            }
            entry.updated_at_ms = session.updated_at_ms.unwrap_or(now);
            entry.token_count = session.token_count.or(entry.token_count);
            entry.resumable = session.resumable;

            entry.clone()
        })
    }

    pub fn update<F>(&self, session_id: &SessionId, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut SessionRecord),
    {
        self.read_modify_write(|records| {
            if let Some(record) = records.get_mut(&session_id.0) {
                update(record);
            }
        })
    }

    pub fn set_alias(&self, session_id: &SessionId, alias: String) -> Result<(), String> {
        self.update(session_id, |record| {
            record.alias = Some(alias);
            record.updated_at_ms = now_ms();
        })
    }

    pub fn set_alias_if_missing(
        &self,
        session_id: &SessionId,
        alias: String,
    ) -> Result<(), String> {
        self.update(session_id, |record| {
            if record.alias.is_none() {
                record.alias = Some(alias);
                record.updated_at_ms = now_ms();
            }
        })
    }

    pub fn set_user_alias(&self, session_id: &SessionId, user_alias: String) -> Result<(), String> {
        self.update(session_id, |record| {
            record.user_alias = Some(user_alias);
            record.updated_at_ms = now_ms();
        })
    }

    pub fn set_generated_alias_if_no_user_alias(
        &self,
        session_id: &SessionId,
        alias: String,
    ) -> Result<bool, String> {
        self.read_modify_write(|records| {
            let Some(record) = records.get_mut(&session_id.0) else {
                return false;
            };
            if record.user_alias.is_some() {
                return false;
            }

            if record.alias.as_deref() != Some(alias.as_str()) {
                record.alias = Some(alias);
                record.updated_at_ms = now_ms();
            }
            true
        })
    }

    pub fn set_session_settings(
        &self,
        session_id: &SessionId,
        settings: SessionSettingsValues,
    ) -> Result<(), String> {
        self.update(session_id, |record| {
            record.session_settings = Some(settings);
            record.updated_at_ms = now_ms();
        })
    }

    pub fn delete(&self, session_id: &SessionId) -> Result<(), String> {
        self.read_modify_write(|records| {
            records.remove(&session_id.0);
        })
    }

    pub fn effective_name(&self, session_id: &SessionId) -> Option<String> {
        self.get(session_id)
            .and_then(|record| record.user_alias.or(record.alias))
    }

    pub fn summaries(&self) -> Result<Vec<SessionSummary>, String> {
        let records = self.list()?;
        Ok(records
            .into_iter()
            .map(|record| SessionSummary {
                id: record.id,
                backend_kind: record.backend_kind,
                workspace_roots: record.workspace_roots,
                project_id: record.project_id,
                alias: record.alias,
                user_alias: record.user_alias,
                parent_id: record.parent_id,
                created_at_ms: record.created_at_ms,
                updated_at_ms: record.updated_at_ms,
                message_count: record.message_count,
                token_count: record.token_count,
                resumable: record.resumable,
            })
            .collect())
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, SessionRecord>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<StoreFile>(&contents)
                .map(|store| store.records)
                .map_err(|err| format!("Failed to parse session store {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read session store {}: {err}",
                path.display()
            )),
        }
    }

    fn read_modify_write<T, F>(&self, modify: F) -> Result<T, String>
    where
        F: FnOnce(&mut HashMap<String, SessionRecord>) -> T,
    {
        let mut records = Self::read_from_disk(&self.path)?;
        let result = modify(&mut records);
        Self::save(&self.path, &records)?;
        Ok(result)
    }

    fn save(path: &Path, records: &HashMap<String, SessionRecord>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
        .map_err(|err| format!("Failed to serialize session store: {err}"))?;

        let parent = path
            .parent()
            .ok_or_else(|| format!("Session store path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create session store directory: {err}"))?;

        let tmp_path = parent.join(format!(
            ".{}.tmp.{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("sessions.json"),
            now_ms()
        ));
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp session store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp session store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp session store file: {err}"))?;
        std::fs::rename(&tmp_path, path).map_err(|err| {
            format!(
                "Failed to atomically replace session store {}: {err}",
                path.display()
            )
        })?;
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}
