use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex as SyncMutex;
use tokio::sync::{Mutex, Notify};
use tyde_protocol::protocol::ChatEvent;

use crate::admin::AdminManager;
use crate::agent::{
    AgentEventBatch, AgentHandle, AgentInfo, AgentRegistry, Backend, CollectedAgentResult,
};
use crate::conversation_sessions::ConversationSessionRegistry;
use crate::debug_log::DebugEventLog;
use crate::stores::{ProjectRecord, ProjectStore};
use crate::stores::{SessionRecord, SessionStore};
use crate::{AgentId, ToolPolicy};

pub struct ServerState {
    pub agent_registry: Arc<Mutex<AgentRegistry>>,
    pub agent_notify: Arc<Notify>,
    pub admin: Mutex<AdminManager<Box<dyn Backend>>>,
    pub session_store: Arc<SyncMutex<SessionStore>>,
    pub project_store: Arc<SyncMutex<ProjectStore>>,
    pub agent_to_session: Arc<SyncMutex<HashMap<String, String>>>,
    pub debug_event_log: SyncMutex<DebugEventLog>,
}

impl ServerState {
    pub fn new(session_store: SessionStore, project_store: ProjectStore) -> Self {
        Self {
            agent_registry: Arc::new(Mutex::new(AgentRegistry::new())),
            agent_notify: Arc::new(Notify::new()),
            admin: Mutex::new(AdminManager::new()),
            session_store: Arc::new(SyncMutex::new(session_store)),
            project_store: Arc::new(SyncMutex::new(project_store)),
            agent_to_session: Arc::new(SyncMutex::new(HashMap::new())),
            debug_event_log: SyncMutex::new(DebugEventLog::new()),
        }
    }

    // ── Agent registration ──────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn register_agent(
        &self,
        backend: Box<dyn Backend>,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let mut reg = self.agent_registry.lock().await;
        let info = reg.register(
            backend,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            ui_owner_project_id,
        );
        drop(reg);
        self.agent_notify.notify_waiters();
        info
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register_agent_with_id(
        &self,
        agent_id: AgentId,
        backend: Box<dyn Backend>,
        workspace_roots: Vec<String>,
        backend_kind: String,
        parent_agent_id: Option<AgentId>,
        name: String,
        ui_owner_project_id: Option<String>,
    ) -> AgentInfo {
        let mut reg = self.agent_registry.lock().await;
        let info = reg.register_with_id(
            agent_id,
            backend,
            workspace_roots,
            backend_kind,
            parent_agent_id,
            name,
            ui_owner_project_id,
        );
        drop(reg);
        self.agent_notify.notify_waiters();
        info
    }

    pub async fn reserve_agent_id(&self) -> AgentId {
        self.agent_registry.lock().await.reserve_agent_id()
    }

    // ── Agent lookups ───────────────────────────────────────────────

    pub async fn has_agent(&self, agent_id: &str) -> bool {
        self.agent_registry.lock().await.has_agent(agent_id)
    }

    pub async fn get_agent(&self, agent_id: &str) -> Option<AgentInfo> {
        self.agent_registry.lock().await.get_info(agent_id)
    }

    pub async fn agent_handle(&self, agent_id: &str) -> Option<AgentHandle> {
        self.agent_registry.lock().await.agent_handle(agent_id)
    }

    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        self.agent_registry.lock().await.list_agents()
    }

    pub async fn children_of(&self, agent_id: &str) -> Vec<AgentInfo> {
        self.agent_registry.lock().await.children_of(agent_id)
    }

    pub async fn active_agent_ids(&self) -> Vec<String> {
        self.agent_registry.lock().await.active_ids()
    }

    pub async fn agent_workspace_roots(&self, agent_id: &str) -> Option<Vec<String>> {
        self.agent_registry.lock().await.workspace_roots(agent_id)
    }

    pub async fn agent_backend_kind(&self, agent_id: &str) -> Option<String> {
        self.agent_registry.lock().await.backend_kind(agent_id)
    }

    pub async fn agent_summaries(&self) -> Vec<(String, String, Vec<String>)> {
        self.agent_registry.lock().await.agent_summaries()
    }

    pub async fn remove_agent(&self, agent_id: &str) -> Option<Box<dyn Backend>> {
        self.agent_registry.lock().await.remove(agent_id)
    }

    pub async fn drain_agents(&self) -> Vec<Box<dyn Backend>> {
        self.agent_registry.lock().await.drain_all()
    }

    // ── Agent mutation ──────────────────────────────────────────────

    pub async fn configure_agent_definition(
        &self,
        agent_id: &str,
        agent_type: Option<String>,
        definition_id: Option<String>,
        tool_policy: ToolPolicy,
    ) -> Option<AgentInfo> {
        let mut reg = self.agent_registry.lock().await;
        let info = reg.configure_agent_definition(agent_id, agent_type, definition_id, tool_policy);
        drop(reg);
        self.agent_notify.notify_waiters();
        info
    }

    pub async fn rename_agent(&self, agent_id: &str, name: String) -> Option<AgentInfo> {
        let mut reg = self.agent_registry.lock().await;
        let _changed = reg.rename_agent(agent_id, name);
        let info = reg.get_info(agent_id);
        drop(reg);
        self.agent_notify.notify_waiters();
        info
    }

    // ── Agent event log ─────────────────────────────────────────────

    pub async fn latest_event_seq_for_agent(&self, agent_id: &str) -> Option<u64> {
        self.agent_registry
            .lock()
            .await
            .latest_event_seq_for_agent(agent_id)
    }

    pub async fn agent_events_since(&self, since_seq: u64, limit: usize) -> AgentEventBatch {
        self.agent_registry
            .lock()
            .await
            .events_since(since_seq, limit)
    }

    pub async fn collect_agent_result(
        &self,
        agent_id: &str,
    ) -> Result<CollectedAgentResult, String> {
        self.agent_registry.lock().await.collect_result(agent_id)
    }

    // ── Snapshots (info + latest event seq) ─────────────────────────

    pub async fn agent_snapshot(&self, agent_id: &str) -> Option<(AgentInfo, Option<u64>)> {
        let reg = self.agent_registry.lock().await;
        let info = reg.get_info(agent_id)?;
        let seq = reg.latest_event_seq_for_agent(agent_id);
        Some((info, seq))
    }

    pub async fn record_chat_event_snapshot(
        &self,
        agent_id: &str,
        event: &ChatEvent,
    ) -> Option<(AgentInfo, Option<u64>)> {
        let mut reg = self.agent_registry.lock().await;
        if !reg.record_chat_event(agent_id, event) {
            return None;
        }
        let info = reg.get_info(agent_id)?;
        let seq = reg.latest_event_seq_for_agent(agent_id);
        drop(reg);
        self.agent_notify.notify_waiters();
        Some((info, seq))
    }

    pub async fn mark_agent_failed_snapshot(
        &self,
        agent_id: &str,
        message: String,
    ) -> Option<(AgentInfo, Option<u64>)> {
        let mut reg = self.agent_registry.lock().await;
        if !reg.mark_agent_failed(agent_id, message) {
            return None;
        }
        let info = reg.get_info(agent_id)?;
        let seq = reg.latest_event_seq_for_agent(agent_id);
        drop(reg);
        self.agent_notify.notify_waiters();
        Some((info, seq))
    }

    pub async fn mark_agent_closed_snapshot(
        &self,
        agent_id: &str,
        message: Option<String>,
    ) -> Option<(AgentInfo, Option<u64>)> {
        let mut reg = self.agent_registry.lock().await;
        if !reg.mark_agent_closed(agent_id, message) {
            return None;
        }
        let info = reg.get_info(agent_id)?;
        let seq = reg.latest_event_seq_for_agent(agent_id);
        drop(reg);
        self.agent_notify.notify_waiters();
        Some((info, seq))
    }

    pub async fn mark_agent_running_snapshot(
        &self,
        agent_id: &str,
        summary: Option<String>,
    ) -> Option<(AgentInfo, Option<u64>)> {
        let mut reg = self.agent_registry.lock().await;
        if !reg.mark_agent_running(agent_id, summary) {
            return None;
        }
        let info = reg.get_info(agent_id)?;
        let seq = reg.latest_event_seq_for_agent(agent_id);
        drop(reg);
        self.agent_notify.notify_waiters();
        Some((info, seq))
    }

    // ── Admin sessions ──────────────────────────────────────────────

    pub async fn create_admin_session(&self, backend: Box<dyn Backend>) -> u64 {
        self.admin.lock().await.create(backend)
    }

    pub async fn remove_admin_session(&self, admin_id: u64) -> Option<Box<dyn Backend>> {
        self.admin.lock().await.remove(admin_id)
    }

    pub async fn active_admin_ids(&self) -> Vec<u64> {
        self.admin.lock().await.active_ids()
    }

    pub async fn admin_handle(&self, admin_id: u64) -> Option<AgentHandle> {
        let admin = self.admin.lock().await;
        admin.get(admin_id).map(|b| b.agent_handle())
    }

    pub async fn admin_kind_str(&self, admin_id: u64) -> Option<String> {
        let admin = self.admin.lock().await;
        admin.get(admin_id).map(|b| b.kind_str())
    }

    pub async fn drain_admin_sessions(&self) -> Vec<Box<dyn Backend>> {
        self.admin.lock().await.drain_all()
    }

    // ── Session records ─────────────────────────────────────────────

    pub fn list_session_records(
        &self,
        workspace_root: Option<&str>,
    ) -> Result<Vec<SessionRecord>, String> {
        let mut records = self.session_store.lock().list()?;
        if let Some(workspace_root) = workspace_root {
            let key = workspace_root_compare_key(workspace_root);
            if !key.is_empty() {
                records.retain(|record| {
                    record
                        .workspace_root
                        .as_deref()
                        .map(workspace_root_compare_key)
                        .as_deref()
                        == Some(key.as_str())
                });
            }
        }
        Ok(records)
    }

    pub fn session_record(&self, id: &str) -> Option<SessionRecord> {
        self.session_store.lock().get(id).cloned()
    }

    pub fn rename_session(&self, id: &str, name: &str) -> Result<(), String> {
        self.session_store.lock().set_user_alias(id, name)
    }

    pub fn set_session_alias(&self, id: &str, alias: &str) -> Result<(), String> {
        self.session_store.lock().set_alias(id, alias)
    }

    pub fn delete_session_record(&self, id: &str) -> Result<(), String> {
        self.session_store.lock().delete(id)
    }

    pub fn delete_session_record_by_backend_session(
        &self,
        backend_kind: &str,
        backend_session_id: &str,
    ) -> Result<(), String> {
        let record_id = {
            let mut store = self.session_store.lock();
            store
                .get_by_backend_session(backend_kind, backend_session_id)
                .map(|r| r.id.clone())
        };
        if let Some(record_id) = record_id {
            self.session_store.lock().delete(&record_id)?;
        }
        Ok(())
    }

    pub fn session_id_for_agent(&self, agent_id: &str) -> Option<String> {
        self.agent_sessions().session_id_for_agent(agent_id)
    }

    pub fn set_agent_session(&self, agent_id: String, session_id: String) {
        self.agent_sessions()
            .set_agent_session(agent_id, session_id);
    }

    pub fn clear_agent_session(&self, agent_id: &str) {
        self.agent_sessions().clear_agent_session(agent_id);
    }

    pub fn clear_all_agent_sessions(&self) {
        self.agent_sessions().clear_all();
    }

    pub fn agent_sessions(&self) -> ConversationSessionRegistry {
        ConversationSessionRegistry::new(self.session_store.clone(), self.agent_to_session.clone())
    }

    pub fn backend_session_id_for_agent(&self, agent_id: &str) -> Option<String> {
        self.agent_sessions().backend_session_id_for_agent(agent_id)
    }

    // ── Projects ────────────────────────────────────────────────────

    pub fn list_projects(&self) -> Result<Vec<ProjectRecord>, String> {
        self.project_store.lock().list()
    }
    pub fn add_project(&self, workspace_path: &str, name: &str) -> Result<ProjectRecord, String> {
        self.project_store.lock().add(workspace_path, name)
    }
    pub fn add_project_workbench(
        &self,
        parent_project_id: &str,
        workspace_path: &str,
        name: &str,
        kind: &str,
    ) -> Result<ProjectRecord, String> {
        self.project_store
            .lock()
            .add_workbench(parent_project_id, workspace_path, name, kind)
    }
    pub fn remove_project(&self, id: &str) -> Result<(), String> {
        self.project_store.lock().remove(id)
    }
    pub fn rename_project(&self, id: &str, name: &str) -> Result<(), String> {
        self.project_store.lock().rename(id, name)
    }
    pub fn update_project_roots(&self, id: &str, roots: Vec<String>) -> Result<(), String> {
        self.project_store.lock().update_roots(id, roots)
    }

    pub fn register_git_workbench(
        &self,
        parent_workspace_path: &str,
        worktree_path: &str,
        branch: &str,
    ) -> Result<(), String> {
        let mut store = self.project_store.lock();
        let parent_id = if let Some(record) = store.get_by_workspace_path(parent_workspace_path) {
            record.id.clone()
        } else {
            let parent_name = parent_workspace_path
                .rsplit('/')
                .next()
                .unwrap_or(parent_workspace_path);
            store.add(parent_workspace_path, parent_name)?.id
        };
        store.add_workbench(&parent_id, worktree_path, branch, "git-worktree")?;
        Ok(())
    }

    pub fn remove_project_by_workspace_path(&self, workspace_path: &str) -> Result<bool, String> {
        let mut store = self.project_store.lock();
        let Some(project_id) = store
            .get_by_workspace_path(workspace_path)
            .map(|r| r.id.clone())
        else {
            return Ok(false);
        };
        store.remove(&project_id)?;
        Ok(true)
    }
}

pub fn workspace_root_compare_key(root: &str) -> String {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(path) = parse_remote_workspace_path(trimmed) {
        return path;
    }
    let mut normalized = trimmed.replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

fn parse_remote_workspace_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !trimmed.starts_with("ssh://") {
        return None;
    }
    let rest = &trimmed["ssh://".len()..];
    let slash_idx = rest.find('/')?;
    let path = rest[slash_idx..].trim();
    if path.is_empty() {
        return None;
    }
    let mut normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::{workspace_root_compare_key, ServerState};
    use crate::stores::{ProjectStore, SessionStore};
    use std::path::PathBuf;

    fn test_server_state() -> ServerState {
        let dir = tempfile::tempdir().unwrap();
        let session_store =
            SessionStore::load(PathBuf::from(dir.path()).join("sessions.json")).unwrap();
        let project_store =
            ProjectStore::load(PathBuf::from(dir.path()).join("projects.json")).unwrap();
        ServerState::new(session_store, project_store)
    }

    #[test]
    fn filters_session_records_by_normalized_workspace_root() {
        let state = test_server_state();
        let record = state
            .session_store
            .lock()
            .create(
                "codex",
                Some("/tmp/project/"),
                &[String::from("/tmp/project/")],
            )
            .unwrap();
        let records = state.list_session_records(Some("/tmp/project")).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, record.id);
    }

    #[test]
    fn normalizes_remote_workspace_compare_key() {
        assert_eq!(
            workspace_root_compare_key("ssh://dev.example.com/tmp/project/"),
            "/tmp/project"
        );
    }

    #[test]
    fn registers_git_workbench_and_auto_adds_parent_project() {
        let state = test_server_state();
        state
            .register_git_workbench("/tmp/project", "/tmp/project--feature", "feature")
            .unwrap();
        let records = state.list_projects().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.workspace_path == "/tmp/project"));
        assert!(records
            .iter()
            .any(|r| r.workspace_path == "/tmp/project--feature"));
    }

    #[test]
    fn removes_project_by_workspace_path() {
        let state = test_server_state();
        let project = state.add_project("/tmp/project", "Project").unwrap();
        assert!(state
            .remove_project_by_workspace_path("/tmp/project")
            .unwrap());
        assert!(!state
            .list_projects()
            .unwrap()
            .iter()
            .any(|r| r.id == project.id));
    }
}
