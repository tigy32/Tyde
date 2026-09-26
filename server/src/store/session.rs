use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use protocol::{
    AgentOrigin, AgentWorkflowMetadata, BackendKind, CompactionMethod, CompactionMetrics,
    CompactionMutation, CompactionOperationId, CompactionTrigger, CustomAgentId, KIRO_BACKEND,
    KIRO_LAUNCH_PROFILE_ID, LEGACY_ACP_BACKEND, LaunchProfileId, ProjectId, SessionId,
    SessionListScope, SessionSettingsValues, SessionSummary, TaskList,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionRestoreState {
    #[serde(default)]
    pub agent_id: Option<protocol::AgentId>,
    pub origin: AgentOrigin,
    #[serde(default)]
    pub workflow: Option<AgentWorkflowMetadata>,
}

/// A new or forked agent whose provider session does not exist yet. It is
/// durable before the agent's first prompt is admitted and carries that
/// prompt, so a host that dies during startup re-issues it on the same agent
/// at the next launch. Startup replaces it with the session record in one
/// transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StartupReservation {
    pub agent_id: protocol::AgentId,
    pub spawn: protocol::SpawnAgentPayload,
    pub origin: AgentOrigin,
    #[serde(default)]
    pub workflow: Option<AgentWorkflowMetadata>,
    pub restorable: bool,
    #[serde(default)]
    pub turn_recovery: Option<TurnRecovery>,
    #[serde(default)]
    pub queued_messages: Vec<protocol::QueuedMessageEntry>,
}

pub(crate) struct CommitCompactedBinding {
    pub operation_id: CompactionOperationId,
    pub expected_generation: u64,
    pub backend_kind: BackendKind,
    pub provider_session_id: SessionId,
    pub metrics: CompactionMetrics,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnRecovery {
    InFlight,
    InterruptedByRestart,
}

impl TurnRecovery {
    pub fn cause(self) -> protocol::RestartInterruptionCause {
        match self {
            Self::InFlight => protocol::RestartInterruptionCause::UnexpectedStop,
            Self::InterruptedByRestart => protocol::RestartInterruptionCause::HostRestart,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub turn_recovery: Option<TurnRecovery>,
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
    /// Present while this saved session owns an open agent card. The actor and
    /// provider process are intentionally not durable; this descriptor lets a
    /// replacement host reconstruct them through the ordinary resume path.
    #[serde(default)]
    pub(crate) restore_state: Option<SessionRestoreState>,
}

#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
    writer: Mutex<Connection>,
}

impl SessionStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        Self::load_with_migration(path).map(|(store, _)| store)
    }

    pub fn database_path(legacy_path: &Path) -> PathBuf {
        legacy_path.with_extension("sqlite3")
    }

    pub fn load_with_migration(legacy_path: PathBuf) -> Result<(Self, HashSet<SessionId>), String> {
        let path = Self::database_path(&legacy_path);
        let parent = path.parent().ok_or("session database has no parent")?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create session directory: {error}"))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create session database: {error}")),
        }
        let mut connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(sql_error)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err("session database requires WAL mode".to_owned());
        }
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let version: u32 = transaction
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(sql_error)?;
        match version {
            0 => {
                transaction.execute_batch(
                    "CREATE TABLE sessions (id TEXT PRIMARY KEY NOT NULL, record TEXT NOT NULL);
                     CREATE TABLE session_tasks (id TEXT PRIMARY KEY NOT NULL, record TEXT NOT NULL);
                     CREATE TABLE archived_sessions (id TEXT PRIMARY KEY NOT NULL, record TEXT NOT NULL);"
                ).map_err(sql_error)?;
                import_json(&transaction, &legacy_path)?;
                transaction
                    .pragma_update(None, "user_version", 1)
                    .map_err(sql_error)?;
            }
            1 => {}
            _ => return Err("session database schema is newer than this server".to_owned()),
        }
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS startup_reservations (id TEXT PRIMARY KEY NOT NULL, record TEXT NOT NULL);",
            )
            .map_err(sql_error)?;
        let purged = {
            let mut statement = transaction
                .prepare("SELECT id FROM archived_sessions")
                .map_err(sql_error)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .map(|row| row.map(SessionId).map_err(sql_error))
                .collect::<Result<HashSet<_>, _>>()?
        };
        transaction.commit().map_err(sql_error)?;
        let store = Self {
            path,
            writer: Mutex::new(connection),
        };
        store.reconcile_incomplete_compactions()?;
        Ok((store, purged))
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
        let records = read_records(&self.reader()?, &[])?;
        let mut out: Vec<_> = records.into_values().collect();
        out.sort_by_key(|record| Reverse(record.updated_at_ms));
        Ok(out)
    }

    pub fn get(&self, id: &SessionId) -> Option<SessionRecord> {
        read_records(&self.reader().ok()?, &[id])
            .ok()?
            .remove(&id.0)
    }

    pub fn get_task_list(&self, id: &SessionId) -> Option<TaskList> {
        let connection = self.reader().ok()?;
        let json: String = connection
            .query_row(
                "SELECT record FROM session_tasks WHERE id=?1",
                [&id.0],
                |row| row.get(0),
            )
            .ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn set_task_list(&self, id: &SessionId, task_list: TaskList) -> Result<(), String> {
        let json = serde_json::to_string(&task_list)
            .map_err(|error| format!("encode task list: {error}"))?;
        let connection = self.writer.lock().map_err(|_| "session writer poisoned")?;
        connection.execute("INSERT INTO session_tasks (id, record) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![id.0, json]).map_err(sql_error)?;
        Ok(())
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
        self.read_modify_write(&[&session.id], |records| {
            let entry = records
                .entry(session.id.0.clone())
                .or_insert_with(|| SessionRecord {
                    turn_recovery: None,
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
                    restore_state: None,
                });

            entry.backend_kind = session.backend_kind;
            if launch_profile_id.is_some() {
                entry.launch_profile_id = launch_profile_id.clone();
            }
            tracing::info!(
                target: "tyde_session_roots",
                existing_root_count = entry.workspace_roots.len(),
                incoming_root_count = session.workspace_roots.len(),
                roots_changed = entry.workspace_roots != session.workspace_roots,
                project_changed = entry.project_id != project_id,
                "Session startup upsert workspace roots"
            );
            entry.workspace_roots = session.workspace_roots.clone();
            entry.project_id = project_id.clone();
            entry.custom_agent_id = custom_agent_id.clone();
            if entry.alias.is_none() {
                entry.alias = session.title.clone();
            }
            if entry.parent_id.is_none() {
                entry.parent_id = parent_id.clone();
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

            Ok(entry.clone())
        })
    }

    pub fn set_access_mode(
        &self,
        session_id: &SessionId,
        access_mode: protocol::BackendAccessMode,
    ) -> Result<(), String> {
        self.cas(&[session_id], |records| {
            let Some(record) = records.get_mut(&session_id.0) else {
                return Err(format!("Session not found: {}", session_id.0));
            };
            record.access_mode = access_mode;
            Ok(((), true))
        })
    }

    pub fn update<F>(&self, session_id: &SessionId, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut SessionRecord),
    {
        self.cas(&[session_id], |records| {
            let Some(record) = records.get_mut(&session_id.0) else {
                return Ok(((), false));
            };
            update(record);
            Ok(((), true))
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
        self.cas(&[session_id], |records| {
            let Some(record) = records.get_mut(&session_id.0) else {
                return Ok((false, false));
            };
            if record.user_alias.is_some() {
                return Ok((false, false));
            }

            let changed = if record.alias.as_deref() != Some(alias.as_str()) {
                record.alias = Some(alias.clone());
                record.updated_at_ms = now_ms();
                true
            } else {
                false
            };
            Ok((true, changed))
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

    pub fn move_to_project(
        &self,
        session_id: &SessionId,
        project_id: Option<ProjectId>,
        roots: Vec<String>,
    ) -> Result<(), String> {
        self.update(session_id, |record| {
            tracing::info!(
                target: "tyde_session_roots",
                existing_root_count = record.workspace_roots.len(),
                incoming_root_count = roots.len(),
                roots_changed = record.workspace_roots != roots,
                project_changed = record.project_id != project_id,
                "Session move persists workspace roots"
            );
            record.project_id = project_id;
            record.workspace_roots = roots;
            record.updated_at_ms = now_ms();
        })
    }

    pub(crate) fn set_restore_state(
        &self,
        session_id: &SessionId,
        restore_state: SessionRestoreState,
    ) -> Result<(), String> {
        self.update(session_id, |record| {
            record.restore_state = Some(restore_state);
        })
    }

    pub(crate) fn clear_restore_states(
        &self,
        session_ids: &HashSet<SessionId>,
    ) -> Result<(), String> {
        self.cas(&[], |records| {
            let mut changed = false;
            for session_id in session_ids {
                if let Some(record) = records.get_mut(&session_id.0)
                    && record.restore_state.take().is_some()
                {
                    changed = true;
                }
            }
            Ok(((), changed))
        })
    }

    pub fn detach_project(&self, project_id: &ProjectId) -> Result<Vec<SessionId>, String> {
        self.cas(&[], |records| {
            let mut detached = Vec::new();
            for record in records.values_mut() {
                if record.project_id.as_ref() == Some(project_id) {
                    record.project_id = None;
                    detached.push(record.id.clone());
                }
            }
            if detached.is_empty() {
                return Ok((detached, false));
            }
            detached.sort_by(|left, right| left.0.cmp(&right.0));
            Ok((detached, true))
        })
    }

    pub fn delete_for_project(&self, project_id: &ProjectId) -> Result<Vec<SessionId>, String> {
        self.cas(&[], |records| {
            let mut deleted = records
                .values()
                .filter(|record| record.project_id.as_ref() == Some(project_id))
                .map(|record| record.id.clone())
                .collect::<Vec<_>>();
            if deleted.is_empty() {
                return Ok((deleted, false));
            }
            for id in &deleted {
                records.remove(&id.0);
            }
            deleted.sort_by(|left, right| left.0.cmp(&right.0));
            Ok((deleted, true))
        })
    }

    pub fn delete(&self, session_id: &SessionId) -> Result<(), String> {
        self.cas(&[session_id], |records| {
            let changed = records.remove(&session_id.0).is_some();
            Ok(((), changed))
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
        self.read_modify_write(&[old_session_id, new_session_id], |records| {
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
            new_record.compaction_summary_preview = Some(summary_preview.clone());
            new_record.updated_at_ms = now;
            Ok(())
        })
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
        self.read_modify_write(&[session_id], |records| {
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
                *existing = operation.clone();
            } else {
                record.compaction_operations.push(operation.clone());
            }
            record.updated_at_ms = now_ms();
            Ok(())
        })
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
        self.read_modify_write(&[session_id], |records| {
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
            operation.metrics = metrics.clone();
            operation.message = message.clone();
            operation.finished_at_ms = Some(now_ms());
            record.compaction_epoch = record.compaction_epoch.saturating_add(1);
            record.updated_at_ms = now_ms();
            Ok(operation.clone())
        })
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
        self.read_modify_write(&[session_id], |records| {
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
                provider_session_id: provider_session_id.clone(),
                created_at_ms: now_ms(),
                created_by_compaction: Some(operation_id.clone()),
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
            operation.metrics = metrics.clone();
            operation.message = message.clone();
            operation.finished_at_ms = Some(now_ms());
            Ok((binding, operation.clone()))
        })
    }

    pub(crate) fn reconcile_incomplete_compactions(
        &self,
    ) -> Result<Vec<CompactionOperationRecord>, String> {
        self.cas(&[], |records| {
            let mut reconciled = Vec::new();
            let mut changed = false;
            for record in records.values_mut() {
                let binding_count = record.backend_bindings.len();
                ensure_backend_binding(record);
                if record.backend_bindings.len() != binding_count {
                    changed = true;
                }
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
                    changed = true;
                }
            }
            Ok((reconciled, changed))
        })
    }

    pub fn compacted_successor_chain(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionId>, String> {
        let records = read_records(&self.reader()?, &[])?;
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
        let records = read_records(&self.reader()?, &[])?;
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
        let backend_storage = crate::backend::BackendStorage::new(HashMap::new())?;
        self.summaries_for_scope_with_backend_storage(scope, &backend_storage)
    }

    pub(crate) fn summaries_for_scope_with_backend_storage(
        &self,
        scope: SessionListScope,
        backend_storage: &crate::backend::BackendStorage,
    ) -> Result<Vec<SessionSummary>, String> {
        let records = read_records(&self.reader()?, &[])?;
        let mut summaries: Vec<SessionSummary> = records
            .values()
            .filter(|record| session_record_matches_scope(record, scope))
            .map(|record| {
                let resumable = session_record_is_resumable(record, backend_storage);
                SessionSummary {
                    id: record.id.clone(),
                    backend_kind: record.backend_kind,
                    launch_profile_id: record.launch_profile_id.clone(),
                    workspace_roots: record.workspace_roots.clone(),
                    project_id: record.project_id.clone(),
                    alias: record.alias.clone(),
                    user_alias: record.user_alias.clone(),
                    parent_id: record.parent_id.clone(),
                    created_at_ms: record.created_at_ms,
                    updated_at_ms: record.updated_at_ms,
                    message_count: record.message_count,
                    token_count: record.token_count,
                    resumable,
                    compacted_from_session_id: record.compacted_from_session_id.clone(),
                    compacted_to_session_id: record.compacted_to_session_id.clone(),
                    compacted_at_ms: record.compacted_at_ms,
                    compaction_summary_preview: record.compaction_summary_preview.clone(),
                }
            })
            .collect();
        summaries.sort_by_key(|summary| Reverse(summary.updated_at_ms));
        Ok(summaries)
    }

    /// Records `reservation`, keeping the messages already queued on an
    /// earlier reservation of the same agent, and returns them.
    pub(crate) fn reserve_startup(
        &self,
        mut reservation: StartupReservation,
    ) -> Result<Vec<protocol::QueuedMessageEntry>, String> {
        self.reservation_transaction(|transaction| {
            if let Some(existing) = read_reservation(transaction, &reservation.agent_id)? {
                reservation.queued_messages = existing.queued_messages;
            }
            write_reservation(transaction, &reservation)?;
            Ok((reservation.queued_messages, true))
        })
    }

    pub(crate) fn update_startup_reservation(
        &self,
        agent_id: &protocol::AgentId,
        update: impl FnOnce(&mut StartupReservation),
    ) -> Result<(), String> {
        self.reservation_transaction(|transaction| {
            let mut reservation = read_reservation(transaction, agent_id)?
                .ok_or("the agent's startup reservation is missing")?;
            update(&mut reservation);
            write_reservation(transaction, &reservation)?;
            Ok(((), true))
        })
    }

    pub(crate) fn remove_startup_reservation(
        &self,
        agent_id: &protocol::AgentId,
    ) -> Result<(), String> {
        self.reservation_transaction(|transaction| {
            let removed = transaction
                .execute(
                    "DELETE FROM startup_reservations WHERE id=?1",
                    [&agent_id.0],
                )
                .map_err(sql_error)?;
            Ok(((), removed > 0))
        })
    }

    pub(crate) fn startup_reservations(&self) -> Result<Vec<StartupReservation>, String> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare("SELECT record FROM startup_reservations")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?;
        rows.map(|row| {
            serde_json::from_str(&row.map_err(sql_error)?)
                .map_err(|error| format!("decode startup reservation: {error}"))
        })
        .collect()
    }

    /// Replaces the agent's startup reservation with its now-existing session:
    /// the reservation's recovery marker, queue and restoration intent move to
    /// the session in the same transaction that removes the reservation, so a
    /// restart finds exactly one of them.
    pub(crate) fn promote_startup_reservation(
        &self,
        agent_id: &protocol::AgentId,
        session_id: &SessionId,
        restore_state: Option<SessionRestoreState>,
    ) -> Result<(), String> {
        self.reservation_transaction(|transaction| {
            let reservation = read_reservation(transaction, agent_id)?
                .ok_or("the agent's startup reservation is missing")?;
            let original = read_raw_records(transaction, &[session_id])?;
            let mut records = decode_records(&original)?;
            let record = records
                .get_mut(&session_id.0)
                .ok_or("the agent's session record is missing")?;
            record.turn_recovery = reservation.turn_recovery;
            record.queued_messages = reservation.queued_messages;
            if restore_state.is_some() {
                record.restore_state = restore_state;
            }
            write_records(transaction, &original, records)?;
            transaction
                .execute(
                    "DELETE FROM startup_reservations WHERE id=?1",
                    [&agent_id.0],
                )
                .map_err(sql_error)?;
            Ok(((), true))
        })
    }

    fn reservation_transaction<T>(
        &self,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        let mut connection = self.writer.lock().map_err(|_| "session writer poisoned")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let (result, wrote) = body(&transaction)?;
        #[cfg(feature = "test-support")]
        if wrote {
            commit_hooks::run(&self.path)?;
        }
        transaction.commit().map_err(sql_error)?;
        tracing::debug!(wrote, "committed startup reservation transaction");
        Ok(result)
    }

    fn reader(&self) -> Result<Connection, String> {
        let connection = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        Ok(connection)
    }

    fn read_modify_write<T>(
        &self,
        ids: &[&SessionId],
        modify: impl FnOnce(&mut HashMap<String, SessionRecord>) -> Result<T, String>,
    ) -> Result<T, String> {
        self.cas(ids, |records| Ok((modify(records)?, true)))
    }

    fn cas<T>(
        &self,
        ids: &[&SessionId],
        body: impl FnOnce(&mut HashMap<String, SessionRecord>) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        let started = Instant::now();
        let mut connection = self.writer.lock().map_err(|_| "session writer poisoned")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let acquired = Instant::now();
        let original = read_raw_records(&transaction, ids)?;
        let mut records = decode_records(&original)?;
        let (result, changed) = body(&mut records)?;
        let writes = if changed {
            write_records(&transaction, &original, records)?
        } else {
            0
        };
        #[cfg(feature = "test-support")]
        if writes > 0 {
            commit_hooks::run(&self.path)?;
        }
        transaction.commit().map_err(sql_error)?;
        tracing::debug!(
            wait_ms = acquired.duration_since(started).as_millis(),
            transaction_ms = acquired.elapsed().as_millis(),
            records_written = writes,
            "committed session transaction"
        );
        Ok(result)
    }
}

#[derive(Debug)]
pub struct SessionStoreHandle {
    store: Arc<SessionStore>,
    readers: Arc<tokio::sync::Semaphore>,
    writers: Arc<tokio::sync::Semaphore>,
}

impl SessionStoreHandle {
    pub(crate) fn new(store: SessionStore) -> Self {
        Self {
            store: Arc::new(store),
            readers: Arc::new(tokio::sync::Semaphore::new(4)),
            writers: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub(crate) async fn call<T, F>(&self, operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&SessionStore) -> Result<T, String> + Send + 'static,
    {
        self.run(Arc::clone(&self.writers), operation).await
    }

    async fn read<T, F>(&self, operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&SessionStore) -> Result<T, String> + Send + 'static,
    {
        self.run(Arc::clone(&self.readers), operation).await
    }

    async fn run<T, F>(
        &self,
        admission: Arc<tokio::sync::Semaphore>,
        operation: F,
    ) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&SessionStore) -> Result<T, String> + Send + 'static,
    {
        let permit = admission
            .acquire_owned()
            .await
            .map_err(|_| "session storage closed")?;
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let result = operation(&store);
            drop(permit);
            result
        })
        .await
        .map_err(|error| format!("session storage task failed: {error}"))?
    }

    pub async fn update<F>(&self, session_id: &SessionId, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut SessionRecord) + Send + 'static,
    {
        let session_id = session_id.clone();
        self.call(move |store| store.update(&session_id, update))
            .await
    }

    pub async fn list(&self) -> Result<Vec<SessionRecord>, String> {
        self.read(move |store| store.list()).await
    }

    pub async fn get(&self, id: &SessionId) -> Option<SessionRecord> {
        let id = id.clone();
        self.read(move |store| Ok(store.get(&id)))
            .await
            .ok()
            .flatten()
    }

    pub async fn get_task_list(&self, id: &SessionId) -> Option<TaskList> {
        let id = id.clone();
        self.read(move |store| Ok(store.get_task_list(&id)))
            .await
            .ok()
            .flatten()
    }

    pub async fn set_task_list(&self, id: &SessionId, task_list: TaskList) -> Result<(), String> {
        let id = id.clone();
        self.call(move |store| store.set_task_list(&id, task_list))
            .await
    }

    pub async fn upsert_backend_session(
        &self,
        session: &BackendSession,
        parent_id: Option<SessionId>,
        project_id: Option<ProjectId>,
        custom_agent_id: Option<CustomAgentId>,
        launch_profile_id: Option<LaunchProfileId>,
    ) -> Result<SessionRecord, String> {
        let session = session.clone();
        self.call(move |store| {
            store.upsert_backend_session(
                &session,
                parent_id,
                project_id,
                custom_agent_id,
                launch_profile_id,
            )
        })
        .await
    }

    pub async fn set_access_mode(
        &self,
        session_id: &SessionId,
        access_mode: protocol::BackendAccessMode,
    ) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_access_mode(&session_id, access_mode))
            .await
    }

    pub async fn set_alias(&self, session_id: &SessionId, alias: String) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_alias(&session_id, alias))
            .await
    }

    pub async fn set_alias_if_missing(
        &self,
        session_id: &SessionId,
        alias: String,
    ) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_alias_if_missing(&session_id, alias))
            .await
    }

    pub async fn set_user_alias(
        &self,
        session_id: &SessionId,
        user_alias: String,
    ) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_user_alias(&session_id, user_alias))
            .await
    }

    pub async fn set_generated_alias_if_no_user_alias(
        &self,
        session_id: &SessionId,
        alias: String,
    ) -> Result<bool, String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_generated_alias_if_no_user_alias(&session_id, alias))
            .await
    }

    pub async fn set_session_settings(
        &self,
        session_id: &SessionId,
        settings: SessionSettingsValues,
    ) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_session_settings(&session_id, settings))
            .await
    }

    pub async fn move_to_project(
        &self,
        session_id: &SessionId,
        project_id: Option<ProjectId>,
        roots: Vec<String>,
    ) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.move_to_project(&session_id, project_id, roots))
            .await
    }

    pub(crate) async fn set_restore_state(
        &self,
        session_id: &SessionId,
        restore_state: SessionRestoreState,
    ) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.set_restore_state(&session_id, restore_state))
            .await
    }

    pub(crate) async fn reserve_startup(
        &self,
        reservation: StartupReservation,
    ) -> Result<Vec<protocol::QueuedMessageEntry>, String> {
        self.call(move |store| store.reserve_startup(reservation))
            .await
    }

    pub(crate) async fn update_startup_reservation(
        &self,
        agent_id: &protocol::AgentId,
        update: impl FnOnce(&mut StartupReservation) + Send + 'static,
    ) -> Result<(), String> {
        let agent_id = agent_id.clone();
        self.call(move |store| store.update_startup_reservation(&agent_id, update))
            .await
    }

    pub(crate) async fn remove_startup_reservation(
        &self,
        agent_id: &protocol::AgentId,
    ) -> Result<(), String> {
        let agent_id = agent_id.clone();
        self.call(move |store| store.remove_startup_reservation(&agent_id))
            .await
    }

    pub(crate) async fn startup_reservations(&self) -> Result<Vec<StartupReservation>, String> {
        self.read(|store| store.startup_reservations()).await
    }

    pub(crate) async fn promote_startup_reservation(
        &self,
        agent_id: &protocol::AgentId,
        session_id: &SessionId,
        restore_state: Option<SessionRestoreState>,
    ) -> Result<(), String> {
        let agent_id = agent_id.clone();
        let session_id = session_id.clone();
        self.call(move |store| {
            store.promote_startup_reservation(&agent_id, &session_id, restore_state)
        })
        .await
    }

    pub(crate) async fn clear_restore_states(
        &self,
        session_ids: &HashSet<SessionId>,
    ) -> Result<(), String> {
        let session_ids = session_ids.clone();
        self.call(move |store| store.clear_restore_states(&session_ids))
            .await
    }

    pub async fn detach_project(&self, project_id: &ProjectId) -> Result<Vec<SessionId>, String> {
        let project_id = project_id.clone();
        self.call(move |store| store.detach_project(&project_id))
            .await
    }

    pub async fn delete_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<SessionId>, String> {
        let project_id = project_id.clone();
        self.call(move |store| store.delete_for_project(&project_id))
            .await
    }

    pub async fn delete(&self, session_id: &SessionId) -> Result<(), String> {
        let session_id = session_id.clone();
        self.call(move |store| store.delete(&session_id)).await
    }

    pub async fn mark_compacted(
        &self,
        old_session_id: &SessionId,
        new_session_id: &SessionId,
        summary_preview: String,
    ) -> Result<(), String> {
        let old_session_id = old_session_id.clone();
        let new_session_id = new_session_id.clone();
        self.call(move |store| {
            store.mark_compacted(&old_session_id, &new_session_id, summary_preview)
        })
        .await
    }

    pub async fn compacted_successor_chain(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionId>, String> {
        let session_id = session_id.clone();
        self.read(move |store| store.compacted_successor_chain(&session_id))
            .await
    }

    pub async fn compacted_ancestor_chain(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionId>, String> {
        let session_id = session_id.clone();
        self.read(move |store| store.compacted_ancestor_chain(&session_id))
            .await
    }

    pub async fn effective_name(&self, session_id: &SessionId) -> Option<String> {
        let session_id = session_id.clone();
        self.read(move |store| Ok(store.effective_name(&session_id)))
            .await
            .ok()
            .flatten()
    }

    pub async fn summaries(&self) -> Result<Vec<SessionSummary>, String> {
        self.read(move |store| store.summaries()).await
    }

    pub async fn summaries_for_scope(
        &self,
        scope: SessionListScope,
    ) -> Result<Vec<SessionSummary>, String> {
        self.read(move |store| store.summaries_for_scope(scope))
            .await
    }

    pub(crate) async fn summaries_for_scope_with_backend_storage(
        &self,
        scope: SessionListScope,
        backend_storage: &crate::backend::BackendStorage,
    ) -> Result<Vec<SessionSummary>, String> {
        let backend_storage = backend_storage.clone();
        self.read(move |store| {
            store.summaries_for_scope_with_backend_storage(scope, &backend_storage)
        })
        .await
    }
}

fn sql_error(error: rusqlite::Error) -> String {
    format!("session database: {error}")
}

fn read_raw_records(
    connection: &Connection,
    ids: &[&SessionId],
) -> Result<HashMap<String, Value>, String> {
    let mut records = HashMap::new();
    if ids.is_empty() {
        let mut statement = connection
            .prepare("SELECT id, record FROM sessions")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_error)?;
        for row in rows {
            let (id, json) = row.map_err(sql_error)?;
            records.insert(
                id,
                serde_json::from_str(&json)
                    .map_err(|error| format!("decode session JSON: {error}"))?,
            );
        }
    } else {
        let mut statement = connection
            .prepare("SELECT record FROM sessions WHERE id=?1")
            .map_err(sql_error)?;
        for id in ids {
            let json: Option<String> = statement
                .query_row([&id.0], |row| row.get(0))
                .optional()
                .map_err(sql_error)?;
            if let Some(json) = json {
                records.insert(
                    id.0.clone(),
                    serde_json::from_str(&json)
                        .map_err(|error| format!("decode session JSON: {error}"))?,
                );
            }
        }
    }
    Ok(records)
}

fn decode_records(raw: &HashMap<String, Value>) -> Result<HashMap<String, SessionRecord>, String> {
    raw.iter()
        .map(|(id, value)| {
            let mut record: SessionRecord = serde_json::from_value(value.clone())
                .map_err(|error| format!("decode session record: {error}"))?;
            if record.id.0 != *id {
                return Err("session record ID differs from its key".to_owned());
            }
            ensure_backend_binding(&mut record);
            Ok((id.clone(), record))
        })
        .collect()
}

fn read_records(
    connection: &Connection,
    ids: &[&SessionId],
) -> Result<HashMap<String, SessionRecord>, String> {
    decode_records(&read_raw_records(connection, ids)?)
}

fn write_records(
    transaction: &rusqlite::Transaction<'_>,
    original: &HashMap<String, Value>,
    records: HashMap<String, SessionRecord>,
) -> Result<usize, String> {
    let mut writes = 0;
    for id in original.keys().filter(|id| !records.contains_key(*id)) {
        transaction
            .execute("DELETE FROM sessions WHERE id=?1", [id])
            .map_err(sql_error)?;
        transaction
            .execute("DELETE FROM session_tasks WHERE id=?1", [id])
            .map_err(sql_error)?;
        writes += 1;
    }
    for (id, record) in records {
        let mut value = original
            .get(&id)
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        merge_record(&mut value, &record)?;
        if original.get(&id) == Some(&value) {
            continue;
        }
        transaction.execute("INSERT INTO sessions (id, record) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![id, value.to_string()]).map_err(sql_error)?;
        writes += 1;
    }
    Ok(writes)
}

fn read_reservation(
    connection: &Connection,
    agent_id: &protocol::AgentId,
) -> Result<Option<StartupReservation>, String> {
    connection
        .query_row(
            "SELECT record FROM startup_reservations WHERE id=?1",
            [&agent_id.0],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error)?
        .map(|json| {
            serde_json::from_str(&json)
                .map_err(|error| format!("decode startup reservation: {error}"))
        })
        .transpose()
}

fn write_reservation(
    connection: &Connection,
    reservation: &StartupReservation,
) -> Result<(), String> {
    let json = serde_json::to_string(reservation)
        .map_err(|error| format!("encode startup reservation: {error}"))?;
    connection
        .execute(
            "INSERT INTO startup_reservations (id, record) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![reservation.agent_id.0, json],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn merge_record(value: &mut Value, record: &SessionRecord) -> Result<(), String> {
    let encoded =
        serde_json::to_value(record).map_err(|error| format!("encode session: {error}"))?;
    let object = value
        .as_object_mut()
        .ok_or("session record must be an object")?;
    object.extend(
        encoded
            .as_object()
            .ok_or("encoded session must be an object")?
            .clone(),
    );
    Ok(())
}

fn legacy_records(path: &Path) -> Result<serde_json::Map<String, Value>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(format!("read legacy session data: {error}")),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse legacy session data: {error}"))?;
    value
        .get("records")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| "legacy session records must be an object".to_owned())
}

fn import_json(connection: &Connection, path: &Path) -> Result<(), String> {
    let records = legacy_records(path)?;
    let tasks = legacy_records(&path.with_extension("task-lists.json"))?;
    let mut imported = 0;
    let mut archived = 0;
    for (id, mut value) in records {
        let object = value
            .as_object_mut()
            .ok_or("legacy session record must be an object")?;
        if object.get("backend_kind").and_then(Value::as_str) == Some("gemini") {
            connection
                .execute(
                    "INSERT INTO archived_sessions VALUES (?1, ?2)",
                    params![id, value.to_string()],
                )
                .map_err(sql_error)?;
            archived += 1;
            continue;
        }
        match object.get("backend_kind").and_then(Value::as_str) {
            Some(LEGACY_ACP_BACKEND) => {
                object.insert(
                    "backend_kind".to_owned(),
                    Value::String(KIRO_BACKEND.to_owned()),
                );
            }
            Some(KIRO_BACKEND) if !matches!(object.get("launch_profile_id"), Some(Value::String(profile)) if !profile.trim().is_empty()) =>
            {
                object.insert(
                    "launch_profile_id".to_owned(),
                    Value::String(KIRO_LAUNCH_PROFILE_ID.to_owned()),
                );
            }
            _ => {}
        }
        let mut record: SessionRecord = serde_json::from_value(value.clone())
            .map_err(|error| format!("import session record: {error}"))?;
        if record.id.0 != id {
            return Err("legacy session record ID differs from its key".to_owned());
        }
        if !crate::backend::native_session_id_is_valid(record.backend_kind, &record.id) {
            record.resumable = false;
        }
        ensure_backend_binding(&mut record);
        merge_record(&mut value, &record)?;
        connection
            .execute(
                "INSERT INTO sessions VALUES (?1, ?2)",
                params![id, value.to_string()],
            )
            .map_err(sql_error)?;
        imported += 1;
    }
    let task_count = tasks.len();
    for (id, value) in tasks {
        serde_json::from_value::<TaskList>(value.clone())
            .map_err(|error| format!("import task list: {error}"))?;
        connection
            .execute(
                "INSERT INTO session_tasks VALUES (?1, ?2)",
                params![id, value.to_string()],
            )
            .map_err(sql_error)?;
    }
    tracing::info!(
        sessions = imported,
        archived_sessions = archived,
        task_lists = task_count,
        "staged legacy session import; original JSON retained"
    );
    Ok(())
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

pub(crate) fn session_record_is_resumable(
    record: &SessionRecord,
    backend_storage: &crate::backend::BackendStorage,
) -> bool {
    crate::backend::stored_session_is_resumable(
        record.backend_kind,
        &record.id,
        record.resumable,
        record.parent_id.is_some(),
        record.compacted_to_session_id.is_some(),
        backend_storage,
    )
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

#[cfg(feature = "test-support")]
pub mod commit_hooks {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    type Hook = Box<dyn FnOnce() -> Result<(), String> + Send>;
    static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Hook>>> = OnceLock::new();

    pub struct InstalledHook(PathBuf);

    impl InstalledHook {
        pub fn install(path: PathBuf, hook: Hook) -> Self {
            let previous = HOOKS
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .insert(path.clone(), hook);
            assert!(previous.is_none(), "session commit hook already installed");
            Self(path)
        }
    }

    impl Drop for InstalledHook {
        fn drop(&mut self) {
            HOOKS
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .remove(&self.0);
        }
    }

    pub(super) fn run(path: &Path) -> Result<(), String> {
        let hook = HOOKS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .remove(path);
        hook.map_or(Ok(()), |hook| hook())
    }
}
