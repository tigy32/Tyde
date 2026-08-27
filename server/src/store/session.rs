use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    BackendKind, CompactionMethod, CompactionMetrics, CompactionMutation, CompactionOperationId,
    CompactionTrigger, CustomAgentId, KIRO_BACKEND, KIRO_LAUNCH_PROFILE_ID, LEGACY_ACP_BACKEND,
    LaunchProfileId, ProjectId, SessionId, SessionListScope, SessionSettingsValues, SessionSummary,
    TaskList,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::backend::BackendSession;

fn default_resumable() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendSessionBinding {
    pub generation: u64,
    pub backend_kind: BackendKind,
    pub provider_session_id: SessionId,
    pub created_at_ms: u64,
    #[serde(default)]
    pub created_by_compaction: Option<CompactionOperationId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredCompactionState {
    Deferred,
    FallbackPreparing,
    NativeDispatchPossible,
    NativeAccepted,
    FallbackCommitPending,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactionOperationRecord {
    pub operation_id: CompactionOperationId,
    pub logical_session_id: SessionId,
    pub trigger: CompactionTrigger,
    pub state: StoredCompactionState,
    #[serde(default)]
    pub method: Option<CompactionMethod>,
    #[serde(default)]
    pub accepted: bool,
    pub mutation: CompactionMutation,
    pub binding_generation_before: u64,
    #[serde(default)]
    pub binding_generation_after: Option<u64>,
    pub transcript_high_water: u64,
    #[serde(default)]
    pub metrics: CompactionMetrics,
    #[serde(default)]
    pub message: Option<String>,
    pub started_at_ms: u64,
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
}

impl CompactionOperationRecord {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            StoredCompactionState::Completed | StoredCompactionState::Failed
        )
    }
}

pub(crate) struct FinishCompactionOperation {
    pub operation_id: CompactionOperationId,
    pub state: StoredCompactionState,
    pub accepted: bool,
    pub mutation: CompactionMutation,
    pub method: Option<CompactionMethod>,
    pub metrics: CompactionMetrics,
    pub message: Option<String>,
}

pub(crate) struct CommitCompactedBinding {
    pub operation_id: CompactionOperationId,
    pub expected_generation: u64,
    pub backend_kind: BackendKind,
    pub provider_session_id: SessionId,
    pub metrics: CompactionMetrics,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub backend_kind: BackendKind,
    #[serde(default)]
    pub launch_profile_id: Option<LaunchProfileId>,
    pub workspace_roots: Vec<String>,
    #[serde(default)]
    pub access_mode: protocol::BackendAccessMode,
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
    /// Persisted assistant responses (one per `StreamEnd`, including partial
    /// responses followed by cancellation or failure); `message_count` is
    /// retained for store compatibility.
    #[serde(default)]
    pub message_count: u32,
    #[serde(default)]
    pub token_count: Option<u64>,
    #[serde(default)]
    pub session_settings: Option<SessionSettingsValues>,
    #[serde(default)]
    pub queued_messages: Vec<protocol::QueuedMessageEntry>,
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
    #[serde(default)]
    pub(crate) backend_bindings: Vec<BackendSessionBinding>,
    #[serde(default)]
    pub active_backend_binding_generation: u64,
    #[serde(default)]
    pub compaction_epoch: u64,
    #[serde(default)]
    pub(crate) compaction_operations: Vec<CompactionOperationRecord>,
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
        Self::migrate_legacy_kiro_sessions(&path)?;
        let _ = Self::read_from_disk(&path)?;
        let store = Self { path };
        let _ = store.reconcile_incomplete_compactions()?;
        Ok((store, purged_gemini_session_ids))
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
                    access_mode: protocol::BackendAccessMode::Unrestricted,
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
                    queued_messages: Vec::new(),
                    resumable: session.resumable,
                    compacted_from_session_id: None,
                    compacted_to_session_id: None,
                    compacted_at_ms: None,
                    compaction_summary_preview: None,
                    backend_bindings: vec![BackendSessionBinding {
                        generation: 0,
                        backend_kind: session.backend_kind,
                        provider_session_id: session.id.clone(),
                        created_at_ms: session.created_at_ms.unwrap_or(now),
                        created_by_compaction: None,
                    }],
                    active_backend_binding_generation: 0,
                    compaction_epoch: 0,
                    compaction_operations: Vec::new(),
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
            if entry.backend_bindings.is_empty() {
                entry.backend_bindings.push(BackendSessionBinding {
                    generation: 0,
                    backend_kind: session.backend_kind,
                    provider_session_id: session.id.clone(),
                    created_at_ms: entry.created_at_ms,
                    created_by_compaction: None,
                });
                entry.active_backend_binding_generation = 0;
            }

            entry.clone()
        })
    }

    pub fn set_access_mode(
        &self,
        session_id: &SessionId,
        access_mode: protocol::BackendAccessMode,
    ) -> Result<(), String> {
        self.read_modify_write(|records| {
            let record = records
                .get_mut(&session_id.0)
                .ok_or_else(|| format!("Session not found: {}", session_id.0))?;
            record.access_mode = access_mode;
            Ok(())
        })?
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

    pub fn delete_for_project(&self, project_id: &ProjectId) -> Result<Vec<SessionId>, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let mut deleted = records
            .values()
            .filter(|record| record.project_id.as_ref() == Some(project_id))
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        if deleted.is_empty() {
            return Ok(deleted);
        }
        for id in &deleted {
            records.remove(&id.0);
        }
        Self::save(&self.path, &records)?;

        let mut task_state = self.read_task_state()?;
        let original_task_count = task_state.records.len();
        for id in &deleted {
            task_state.records.remove(&id.0);
        }
        if task_state.records.len() != original_task_count {
            let value = serde_json::to_value(task_state)
                .map_err(|err| format!("Failed to serialize task state: {err}"))?;
            write_json_value_atomically(&self.task_state_path(), &value)?;
        }
        deleted.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(deleted)
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

    pub(crate) fn compaction_operation(
        &self,
        session_id: &SessionId,
        operation_id: &CompactionOperationId,
    ) -> Option<CompactionOperationRecord> {
        self.get(session_id).and_then(|record| {
            record
                .compaction_operations
                .into_iter()
                .find(|operation| operation.operation_id == *operation_id)
        })
    }

    pub(crate) fn put_compaction_operation(
        &self,
        session_id: &SessionId,
        operation: CompactionOperationRecord,
    ) -> Result<(), String> {
        self.read_modify_write(|records| {
            let record = records
                .get_mut(&session_id.0)
                .ok_or_else(|| format!("missing session {session_id}"))?;
            if let Some(existing) = record
                .compaction_operations
                .iter_mut()
                .find(|existing| existing.operation_id == operation.operation_id)
            {
                if existing.is_terminal() {
                    return Err(format!(
                        "compaction operation {} is already terminal",
                        operation.operation_id.0
                    ));
                }
                *existing = operation;
            } else {
                record.compaction_operations.push(operation);
            }
            record.updated_at_ms = now_ms();
            Ok(())
        })?
    }

    pub(crate) fn finish_compaction_operation(
        &self,
        session_id: &SessionId,
        update: FinishCompactionOperation,
    ) -> Result<CompactionOperationRecord, String> {
        let FinishCompactionOperation {
            operation_id,
            state,
            accepted,
            mutation,
            method,
            metrics,
            message,
        } = update;
        if !matches!(
            state,
            StoredCompactionState::Completed | StoredCompactionState::Failed
        ) {
            return Err("terminal compaction state required".to_owned());
        }
        self.read_modify_write(|records| {
            let record = records
                .get_mut(&session_id.0)
                .ok_or_else(|| format!("missing session {session_id}"))?;
            let operation = record
                .compaction_operations
                .iter_mut()
                .find(|operation| operation.operation_id == operation_id)
                .ok_or_else(|| format!("missing compaction operation {}", operation_id.0))?;
            if operation.is_terminal() {
                return Ok(operation.clone());
            }
            operation.state = state;
            operation.accepted = accepted;
            operation.mutation = mutation;
            operation.method = method;
            operation.metrics = metrics;
            operation.message = message;
            operation.finished_at_ms = Some(now_ms());
            record.compaction_epoch = record.compaction_epoch.saturating_add(1);
            record.updated_at_ms = now_ms();
            Ok(operation.clone())
        })?
    }

    pub(crate) fn commit_compacted_binding(
        &self,
        session_id: &SessionId,
        commit: CommitCompactedBinding,
    ) -> Result<(BackendSessionBinding, CompactionOperationRecord), String> {
        let CommitCompactedBinding {
            operation_id,
            expected_generation,
            backend_kind,
            provider_session_id,
            metrics,
            message,
        } = commit;
        self.read_modify_write(|records| {
            let record = records
                .get_mut(&session_id.0)
                .ok_or_else(|| format!("missing session {session_id}"))?;
            ensure_backend_binding(record);
            if record.active_backend_binding_generation != expected_generation {
                return Err(format!(
                    "binding generation changed from {expected_generation} to {}",
                    record.active_backend_binding_generation
                ));
            }
            let operation_index = record
                .compaction_operations
                .iter()
                .position(|operation| operation.operation_id == operation_id)
                .ok_or_else(|| format!("missing compaction operation {}", operation_id.0))?;
            if record.compaction_operations[operation_index].is_terminal() {
                return Err(format!(
                    "compaction operation {} is already terminal",
                    operation_id.0
                ));
            }
            let generation = expected_generation.saturating_add(1);
            let binding = BackendSessionBinding {
                generation,
                backend_kind,
                provider_session_id,
                created_at_ms: now_ms(),
                created_by_compaction: Some(operation_id),
            };
            record.backend_bindings.push(binding.clone());
            record.active_backend_binding_generation = generation;
            record.backend_kind = backend_kind;
            record.compaction_epoch = record.compaction_epoch.saturating_add(1);
            record.updated_at_ms = now_ms();

            let operation = &mut record.compaction_operations[operation_index];
            operation.state = StoredCompactionState::Completed;
            operation.accepted = false;
            operation.mutation = CompactionMutation::Completed;
            operation.method = Some(CompactionMethod::InlineFallback);
            operation.binding_generation_after = Some(generation);
            operation.metrics = metrics;
            operation.message = message;
            operation.finished_at_ms = Some(now_ms());
            Ok((binding, operation.clone()))
        })?
    }

    pub(crate) fn reconcile_incomplete_compactions(
        &self,
    ) -> Result<Vec<CompactionOperationRecord>, String> {
        self.read_modify_write(|records| {
            let mut reconciled = Vec::new();
            for record in records.values_mut() {
                ensure_backend_binding(record);
                let mut record_reconciled = false;
                for operation in &mut record.compaction_operations {
                    if operation.is_terminal() {
                        continue;
                    }
                    let (accepted, mutation, message) = match operation.state {
                        StoredCompactionState::NativeDispatchPossible
                        | StoredCompactionState::NativeAccepted => (
                            true,
                            CompactionMutation::MayHaveMutated,
                            "server restarted while native compaction may have been running",
                        ),
                        _ => (
                            false,
                            CompactionMutation::NotObserved,
                            "server restarted before compaction committed",
                        ),
                    };
                    operation.state = StoredCompactionState::Failed;
                    operation.accepted = accepted;
                    operation.mutation = mutation;
                    operation.message = Some(message.to_owned());
                    operation.finished_at_ms = Some(now_ms());
                    reconciled.push(operation.clone());
                    record_reconciled = true;
                }
                if record_reconciled {
                    record.compaction_epoch = record.compaction_epoch.saturating_add(1);
                    record.updated_at_ms = now_ms();
                }
            }
            reconciled
        })
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

    /// Repoint Kiro sessions at the ACP backend.
    ///
    /// Unlike the Gemini migration these sessions are *not* purged — the
    /// underlying Kiro session files are untouched and still resumable. They
    /// just need the new backend kind, and a launch profile binding so the
    /// backend knows which ACP agent to start. A session that already names a
    /// profile keeps it.
    fn migrate_legacy_kiro_sessions(path: &Path) -> Result<(), String> {
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
        for record in records.values_mut() {
            let Some(record) = record.as_object_mut() else {
                continue;
            };
            // Two legacy shapes converge here, and each needs a different
            // half of the fix. A record spelled `acp` was written after the
            // backend was named for the protocol: it already carries a launch
            // profile, and only the spelling is stale. A record spelled `kiro`
            // predates that rename entirely: the spelling is canonical again,
            // but it was written before the built-in profile existed, so it is
            // the one that needs the profile filled in. Doing both to both
            // would hand the built-in Kiro profile to a modern record whose
            // custom profile was deliberately removed.
            match record.get("backend_kind").and_then(Value::as_str) {
                Some(LEGACY_ACP_BACKEND) => {
                    record.insert(
                        "backend_kind".to_string(),
                        Value::String(KIRO_BACKEND.to_string()),
                    );
                    changed = true;
                }
                Some(KIRO_BACKEND) => {
                    let needs_profile = !matches!(
                        record.get("launch_profile_id"),
                        Some(Value::String(existing)) if !existing.trim().is_empty()
                    );
                    if needs_profile {
                        record.insert(
                            "launch_profile_id".to_string(),
                            Value::String(KIRO_LAUNCH_PROFILE_ID.to_string()),
                        );
                        changed = true;
                    }
                }
                _ => continue,
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
                .map(|mut store| {
                    for record in store.records.values_mut() {
                        ensure_backend_binding(record);
                    }
                    store.records
                })
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

fn ensure_backend_binding(record: &mut SessionRecord) {
    if record.backend_bindings.is_empty() {
        record.backend_bindings.push(BackendSessionBinding {
            generation: 0,
            backend_kind: record.backend_kind,
            provider_session_id: record.id.clone(),
            created_at_ms: record.created_at_ms,
            created_by_compaction: None,
        });
        record.active_backend_binding_generation = 0;
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
        BackendKind::Tycode => false,
        BackendKind::Antigravity => {
            !antigravity_record_is_permanently_non_resumable(record)
                && is_antigravity_resumable(&record.id)
        }
        BackendKind::Kiro | BackendKind::Claude | BackendKind::Codex | BackendKind::Hermes => {
            record.resumable
        }
    }
}

fn antigravity_record_is_permanently_non_resumable(record: &SessionRecord) -> bool {
    record.compacted_to_session_id.is_some() || (!record.resumable && record.parent_id.is_some())
}
