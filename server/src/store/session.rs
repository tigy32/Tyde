use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    BackendKind, CustomAgentId, LaunchProfileId, ProjectId, SessionId, SessionListScope,
    SessionSettingsValues, SessionSummary, TaskList,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::backend::BackendSession;

fn default_resumable() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    #[serde(default)]
    pub launch_profile_id: Option<LaunchProfileId>,
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
    #[serde(default)]
    pub compacted_from_session_id: Option<SessionId>,
    #[serde(default)]
    pub compacted_to_session_id: Option<SessionId>,
    #[serde(default)]
    pub compacted_at_ms: Option<u64>,
    #[serde(default)]
    pub compaction_summary_preview: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, SessionRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TaskStateFile {
    records: HashMap<String, TaskList>,
}

#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        Self::load_with_migration(path).map(|(store, _purged_gemini_session_ids)| store)
    }

    pub fn load_with_migration(path: PathBuf) -> Result<(Self, HashSet<SessionId>), String> {
        let purged_gemini_session_ids = Self::purge_legacy_gemini_sessions(&path)?;
        Self::mark_non_native_antigravity_sessions_non_resumable(&path)?;
        let _ = Self::read_from_disk(&path)?;
        Ok((Self { path }, purged_gemini_session_ids))
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SESSION_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?
            .join(".tyde")
            .join("sessions.json"))
    }

    pub fn list(&self) -> Result<Vec<SessionRecord>, String> {
        let records = Self::read_from_disk(&self.path)?;
        let mut out: Vec<_> = records.into_values().collect();
        out.sort_by_key(|record| Reverse(record.updated_at_ms));
        Ok(out)
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionRecord> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn get_task_list(&self, id: &SessionId) -> Option<TaskList> {
        self.read_task_state()
            .ok()
            .and_then(|state| state.records.get(&id.0).cloned())
    }

    pub fn set_task_list(&self, id: &SessionId, task_list: TaskList) -> Result<(), String> {
        let mut state = self.read_task_state()?;
        state.records.insert(id.0.clone(), task_list);
        let value = serde_json::to_value(state)
            .map_err(|err| format!("Failed to serialize task state: {err}"))?;
        write_json_value_atomically(&self.task_state_path(), &value)
    }

    fn task_state_path(&self) -> PathBuf {
        self.path.with_extension("task-lists.json")
    }

    fn read_task_state(&self) -> Result<TaskStateFile, String> {
        let path = self.task_state_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|err| format!("Failed to parse task state {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(TaskStateFile::default()),
            Err(err) => Err(format!(
                "Failed to read task state {}: {err}",
                path.display()
            )),
        }
    }

    pub fn upsert_backend_session(
        &self,
        session: &BackendSession,
        parent_id: Option<SessionId>,
        project_id: Option<ProjectId>,
        custom_agent_id: Option<CustomAgentId>,
        launch_profile_id: Option<LaunchProfileId>,
    ) -> Result<SessionRecord, String> {
        let now = now_ms();
        self.read_modify_write(|records| {
            let entry = records
                .entry(session.id.0.clone())
                .or_insert_with(|| SessionRecord {
                    id: session.id.clone(),
                    backend_kind: session.backend_kind,
                    launch_profile_id: launch_profile_id.clone(),
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
                    compacted_from_session_id: None,
                    compacted_to_session_id: None,
                    compacted_at_ms: None,
                    compaction_summary_preview: None,
                });

            entry.backend_kind = session.backend_kind;
            if launch_profile_id.is_some() {
                entry.launch_profile_id = launch_profile_id;
            }
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

    pub fn detach_project(&self, project_id: &ProjectId) -> Result<Vec<SessionId>, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let mut detached = Vec::new();
        for record in records.values_mut() {
            if record.project_id.as_ref() == Some(project_id) {
                record.project_id = None;
                detached.push(record.id.clone());
            }
        }
        if !detached.is_empty() {
            Self::save(&self.path, &records)?;
            detached.sort_by(|left, right| left.0.cmp(&right.0));
        }
        Ok(detached)
    }

    pub fn delete(&self, session_id: &SessionId) -> Result<(), String> {
        self.read_modify_write(|records| {
            records.remove(&session_id.0);
        })
    }

    pub fn mark_compacted(
        &self,
        old_session_id: &SessionId,
        new_session_id: &SessionId,
        summary_preview: String,
    ) -> Result<(), String> {
        if old_session_id == new_session_id {
            return Err(format!(
                "cannot compact session {old_session_id} into itself"
            ));
        }
        self.read_modify_write(|records| {
            if !records.contains_key(&old_session_id.0) {
                return Err(format!(
                    "cannot compact missing old session {old_session_id}"
                ));
            }
            if !records.contains_key(&new_session_id.0) {
                return Err(format!(
                    "cannot compact into missing new session {new_session_id}"
                ));
            }

            let now = now_ms();
            let old_record = records
                .get_mut(&old_session_id.0)
                .expect("old session existence checked before compaction mark");
            old_record.resumable = false;
            old_record.compacted_to_session_id = Some(new_session_id.clone());
            old_record.compacted_at_ms = Some(now);
            old_record.compaction_summary_preview = Some(summary_preview.clone());
            old_record.updated_at_ms = now;

            let new_record = records
                .get_mut(&new_session_id.0)
                .expect("new session existence checked before compaction mark");
            new_record.compacted_from_session_id = Some(old_session_id.clone());
            new_record.compacted_at_ms = Some(now);
            new_record.compaction_summary_preview = Some(summary_preview);
            new_record.updated_at_ms = now;
            Ok(())
        })?
    }

    pub fn compacted_successor_chain(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionId>, String> {
        let records = Self::read_from_disk(&self.path)?;
        let mut out = Vec::new();
        let mut current = session_id.clone();
        let mut seen = std::collections::HashSet::new();
        seen.insert(current.clone());
        for _ in 0..16 {
            let Some(record) = records.get(&current.0) else {
                break;
            };
            let Some(next) = record.compacted_to_session_id.clone() else {
                break;
            };
            if !seen.insert(next.clone()) {
                return Err(format!("compacted session lineage loop includes {next}"));
            }
            out.push(next.clone());
            current = next;
        }
        Ok(out)
    }

    pub fn compacted_ancestor_chain(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionId>, String> {
        let records = Self::read_from_disk(&self.path)?;
        let mut out = Vec::new();
        let mut current = session_id.clone();
        let mut seen = std::collections::HashSet::new();
        seen.insert(current.clone());
        for _ in 0..16 {
            let Some(previous) = records
                .values()
                .find(|record| record.compacted_to_session_id.as_ref() == Some(&current))
                .map(|record| record.id.clone())
            else {
                break;
            };
            if !seen.insert(previous.clone()) {
                return Err(format!(
                    "compacted session lineage loop includes {previous}"
                ));
            }
            out.push(previous.clone());
            current = previous;
        }
        Ok(out)
    }

    pub fn effective_name(&self, session_id: &SessionId) -> Option<String> {
        self.get(session_id)
            .and_then(|record| record.user_alias.or(record.alias))
    }

    pub fn summaries(&self) -> Result<Vec<SessionSummary>, String> {
        self.summaries_for_scope(SessionListScope::AllSessions)
    }

    pub fn summaries_for_scope(
        &self,
        scope: SessionListScope,
    ) -> Result<Vec<SessionSummary>, String> {
        let antigravity_conversations_dir =
            crate::backend::antigravity::resolve_antigravity_conversations_dir(None)?;
        self.summaries_for_scope_with_antigravity_conversations_dir(
            scope,
            &antigravity_conversations_dir,
        )
    }

    pub(crate) fn summaries_for_scope_with_antigravity_conversations_dir(
        &self,
        scope: SessionListScope,
        antigravity_conversations_dir: &Path,
    ) -> Result<Vec<SessionSummary>, String> {
        let records = self.list()?;
        Ok(records
            .into_iter()
            .filter(|record| session_record_matches_scope(record, scope))
            .map(|record| {
                let resumable = session_record_is_resumable(&record, antigravity_conversations_dir);
                SessionSummary {
                    id: record.id,
                    backend_kind: record.backend_kind,
                    launch_profile_id: record.launch_profile_id,
                    workspace_roots: record.workspace_roots,
                    project_id: record.project_id,
                    alias: record.alias,
                    user_alias: record.user_alias,
                    parent_id: record.parent_id,
                    created_at_ms: record.created_at_ms,
                    updated_at_ms: record.updated_at_ms,
                    message_count: record.message_count,
                    token_count: record.token_count,
                    resumable,
                    compacted_from_session_id: record.compacted_from_session_id,
                    compacted_to_session_id: record.compacted_to_session_id,
                    compacted_at_ms: record.compacted_at_ms,
                    compaction_summary_preview: record.compaction_summary_preview,
                }
            })
            .collect())
    }

    fn purge_legacy_gemini_sessions(path: &Path) -> Result<HashSet<SessionId>, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
            Err(err) => {
                return Err(format!(
                    "Failed to read session store {}: {err}",
                    path.display()
                ));
            }
        };
        let mut value = serde_json::from_str::<Value>(&contents)
            .map_err(|err| format!("Failed to parse session store {}: {err}", path.display()))?;
        let records = value
            .get_mut("records")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                format!(
                    "Failed to migrate session store {}: records must be an object",
                    path.display()
                )
            })?;
        let mut purged = HashSet::new();
        records.retain(|session_id, record| {
            let is_gemini = record.get("backend_kind").and_then(Value::as_str) == Some("gemini");
            if is_gemini {
                purged.insert(SessionId(session_id.clone()));
                return false;
            }
            true
        });
        if !purged.is_empty() {
            write_json_value_atomically(path, &value).map_err(|err| {
                format!(
                    "Failed to rewrite migrated session store {}: {err}",
                    path.display()
                )
            })?;
        }
        Ok(purged)
    }

    fn mark_non_native_antigravity_sessions_non_resumable(path: &Path) -> Result<(), String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(format!(
                    "Failed to read session store {}: {err}",
                    path.display()
                ));
            }
        };
        let mut value = serde_json::from_str::<Value>(&contents)
            .map_err(|err| format!("Failed to parse session store {}: {err}", path.display()))?;
        let records = value
            .get_mut("records")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                format!(
                    "Failed to migrate session store {}: records must be an object",
                    path.display()
                )
            })?;

        let mut changed = false;
        for (session_id, record) in records {
            let Some(record) = record.as_object_mut() else {
                return Err(format!(
                    "Failed to migrate session store {}: record {session_id} must be an object",
                    path.display()
                ));
            };
            let is_antigravity =
                record.get("backend_kind").and_then(Value::as_str) == Some("antigravity");
            if is_antigravity
                && !is_native_antigravity_session_id(session_id)
                && record.get("resumable").and_then(Value::as_bool) != Some(false)
            {
                record.insert("resumable".to_string(), Value::Bool(false));
                changed = true;
            }
        }

        if changed {
            write_json_value_atomically(path, &value).map_err(|err| {
                format!(
                    "Failed to rewrite migrated session store {}: {err}",
                    path.display()
                )
            })?;
        }
        Ok(())
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

fn write_json_value_atomically(path: &Path, value: &Value) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|err| format!("Failed to serialize migrated session store: {err}"))?;
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
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

fn is_native_antigravity_session_id(session_id: &str) -> bool {
    session_id.len() == 36 && Uuid::parse_str(session_id).is_ok()
}

pub(crate) fn session_record_is_resumable(
    record: &SessionRecord,
    antigravity_conversations_dir: &Path,
) -> bool {
    session_record_is_resumable_with(record, |session_id| {
        crate::backend::antigravity::is_antigravity_session_resumable(
            session_id,
            antigravity_conversations_dir,
        )
    })
}

pub(crate) fn session_summary_matches_scope(
    summary: &SessionSummary,
    scope: SessionListScope,
) -> bool {
    match scope {
        SessionListScope::RootSessions => summary.parent_id.is_none(),
        SessionListScope::AllSessions => true,
    }
}

fn session_record_matches_scope(record: &SessionRecord, scope: SessionListScope) -> bool {
    match scope {
        SessionListScope::RootSessions => record.parent_id.is_none(),
        SessionListScope::AllSessions => true,
    }
}

fn session_record_is_resumable_with<F>(record: &SessionRecord, is_antigravity_resumable: F) -> bool
where
    F: Fn(&SessionId) -> bool,
{
    match record.backend_kind {
        BackendKind::Antigravity => {
            !antigravity_record_is_permanently_non_resumable(record)
                && is_antigravity_resumable(&record.id)
        }
        BackendKind::Tycode
        | BackendKind::Kiro
        | BackendKind::Claude
        | BackendKind::Codex
        | BackendKind::Hermes => record.resumable,
    }
}

fn antigravity_record_is_permanently_non_resumable(record: &SessionRecord) -> bool {
    record.compacted_to_session_id.is_some() || (!record.resumable && record.parent_id.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_store_purges_legacy_gemini_records_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.json");
        std::fs::write(
            &path,
            r#"{
  "records": {
    "gemini-session": {
      "id": "gemini-session",
      "backend_kind": "gemini",
      "workspace_roots": ["/tmp"],
      "created_at_ms": 1,
      "updated_at_ms": 2
    },
    "claude-session": {
      "id": "claude-session",
      "backend_kind": "claude",
      "workspace_roots": ["/tmp"],
      "created_at_ms": 3,
      "updated_at_ms": 4
    }
  }
}"#,
        )
        .expect("write legacy session store");

        let (store, purged) =
            SessionStore::load_with_migration(path.clone()).expect("load migrated session store");
        assert_eq!(
            purged,
            [SessionId("gemini-session".to_string())]
                .into_iter()
                .collect()
        );
        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, SessionId("claude-session".to_string()));
        let rewritten = std::fs::read_to_string(path).expect("read rewritten store");
        assert!(!rewritten.contains("gemini"));
        assert!(rewritten.contains("claude-session"));
    }

    #[test]
    fn session_store_loads_legacy_records_without_compaction_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.json");
        std::fs::write(
            &path,
            r#"{
  "records": {
    "session-1": {
      "id": "session-1",
      "backend_kind": "claude",
      "workspace_roots": [],
      "created_at_ms": 1,
      "updated_at_ms": 2,
      "message_count": 3,
      "resumable": true
    }
  }
}"#,
        )
        .expect("write legacy session store");

        let store = SessionStore::load(path).expect("load legacy session store");
        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert!(summary.resumable);
        assert!(summary.compacted_from_session_id.is_none());
        assert!(summary.compacted_to_session_id.is_none());
        assert!(summary.compacted_at_ms.is_none());
        assert!(summary.compaction_summary_preview.is_none());
    }

    #[test]
    fn session_store_marks_legacy_synthetic_antigravity_sessions_non_resumable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.json");
        std::fs::write(
            &path,
            r#"{
  "records": {
    "antigravity-55a3c5e1-a2e1-44c1-9246-6e3de751803d": {
      "id": "antigravity-55a3c5e1-a2e1-44c1-9246-6e3de751803d",
      "backend_kind": "antigravity",
      "workspace_roots": [],
      "created_at_ms": 1,
      "updated_at_ms": 2,
      "resumable": true
    },
    "55a3c5e1-a2e1-44c1-9246-6e3de751803d": {
      "id": "55a3c5e1-a2e1-44c1-9246-6e3de751803d",
      "backend_kind": "antigravity",
      "workspace_roots": [],
      "created_at_ms": 3,
      "updated_at_ms": 4,
      "resumable": true
    }
  }
}"#,
        )
        .expect("write antigravity session store");

        let store = SessionStore::load(path).expect("load migrated session store");
        let synthetic = store
            .get(&SessionId(
                "antigravity-55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string(),
            ))
            .expect("synthetic antigravity record");
        assert!(!synthetic.resumable);

        let native = store
            .get(&SessionId(
                "55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_string(),
            ))
            .expect("native antigravity record");
        assert!(native.resumable);
    }

    #[test]
    fn session_summaries_mark_native_antigravity_missing_db_non_resumable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.json");
        let session_id = Uuid::new_v4().to_string();
        std::fs::write(
            &path,
            format!(
                r#"{{
  "records": {{
    "{session_id}": {{
      "id": "{session_id}",
      "backend_kind": "antigravity",
      "workspace_roots": [],
      "created_at_ms": 1,
      "updated_at_ms": 2,
      "resumable": true
    }}
  }}
}}"#
            ),
        )
        .expect("write antigravity session store");

        let store = SessionStore::load(path).expect("load session store");
        let summaries = store.summaries().expect("summaries");
        assert_eq!(summaries.len(), 1);
        assert!(
            !summaries[0].resumable,
            "native Antigravity UUID without a backing AGY db must not be emitted resumable"
        );
    }

    #[test]
    fn antigravity_record_resumability_allows_transient_missing_db_to_recover() {
        let session_id = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_owned());
        let record = test_record(BackendKind::Antigravity, session_id.clone(), false);

        assert!(!session_record_is_resumable_with(&record, |_| false));
        assert!(session_record_is_resumable_with(&record, |id| id == &session_id));
    }

    #[test]
    fn antigravity_record_resumability_preserves_permanent_false_records() {
        let old_session_id = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_owned());
        let new_session_id = SessionId("66666666-6666-4666-8666-666666666666".to_owned());
        let parent_session_id = SessionId("77777777-7777-4777-8777-777777777777".to_owned());

        let mut compacted = test_record(BackendKind::Antigravity, old_session_id.clone(), false);
        compacted.compacted_to_session_id = Some(new_session_id);
        assert!(
            !session_record_is_resumable_with(&compacted, |_| true),
            "compacted Antigravity records must stay non-resumable even when a native db exists"
        );

        let mut backend_native = test_record(BackendKind::Antigravity, old_session_id, false);
        backend_native.parent_id = Some(parent_session_id);
        assert!(
            !session_record_is_resumable_with(&backend_native, |_| true),
            "backend-native Antigravity child records must stay non-resumable even when a native db exists"
        );
    }

    #[test]
    fn session_store_round_trips_task_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sessions.json");
        let id = SessionId("task-session".to_string());
        let record = test_record(BackendKind::Codex, id.clone(), true);
        std::fs::write(
            &path,
            serde_json::to_vec(&StoreFile {
                records: HashMap::from([(id.0.clone(), record)]),
            })
            .expect("serialize store"),
        )
        .expect("write store");
        let store = SessionStore::load(path).expect("load store");
        store
            .set_task_list(
                &id,
                TaskList {
                    title: String::new(),
                    tasks: vec![protocol::Task {
                        id: 1,
                        description: "Alpha check".to_string(),
                        status: protocol::TaskStatus::Completed,
                    }],
                },
            )
            .expect("persist task list");

        let task_list = store.get_task_list(&id).expect("stored task list");
        assert_eq!(task_list.tasks[0].description, "Alpha check");
    }

    fn test_record(backend_kind: BackendKind, id: SessionId, resumable: bool) -> SessionRecord {
        SessionRecord {
            id,
            backend_kind,
            launch_profile_id: None,
            workspace_roots: Vec::new(),
            project_id: None,
            custom_agent_id: None,
            alias: None,
            user_alias: None,
            parent_id: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 0,
            token_count: None,
            session_settings: None,
            resumable,
            compacted_from_session_id: None,
            compacted_to_session_id: None,
            compacted_at_ms: None,
            compaction_summary_preview: None,
        }
    }
}
