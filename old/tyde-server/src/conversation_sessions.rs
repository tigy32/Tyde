use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex as SyncMutex;
use tyde_protocol::protocol::{
    ChatEvent, ChatMessage, MessageSender, SessionStartedData, TaskList,
};

use crate::stores::SessionStore;

#[derive(Clone)]
pub struct ConversationSessionRegistry {
    session_store: Arc<SyncMutex<SessionStore>>,
    agent_to_session: Arc<SyncMutex<HashMap<String, String>>>,
}

pub struct ResumeSessionBinding {
    pub created_record_id: Option<String>,
}

impl ConversationSessionRegistry {
    pub fn new(
        session_store: Arc<SyncMutex<SessionStore>>,
        agent_to_session: Arc<SyncMutex<HashMap<String, String>>>,
    ) -> Self {
        Self {
            session_store,
            agent_to_session,
        }
    }

    pub fn session_id_for_agent(&self, agent_id: &str) -> Option<String> {
        self.agent_to_session.lock().get(agent_id).cloned()
    }

    pub fn set_agent_session(&self, agent_id: String, session_id: String) {
        self.agent_to_session.lock().insert(agent_id, session_id);
    }

    pub fn clear_agent_session(&self, agent_id: &str) {
        self.agent_to_session.lock().remove(agent_id);
    }

    pub fn clear_all(&self) {
        self.agent_to_session.lock().clear();
    }

    pub fn create_session_binding(
        &self,
        agent_id: &str,
        backend_kind: &str,
        workspace_root: Option<&str>,
        workspace_roots: &[String],
        parent_agent_id: Option<&str>,
        alias: Option<&str>,
    ) -> Result<String, String> {
        let parent_session_id =
            parent_agent_id.and_then(|parent| self.session_id_for_agent(parent));

        let mut store = self.session_store.lock();
        let record = store.create(backend_kind, workspace_root, workspace_roots)?;
        let session_id = record.id.clone();
        if let Some(parent_id) = parent_session_id.as_deref() {
            store.set_parent(&session_id, parent_id)?;
        }
        if let Some(alias) = alias.map(str::trim).filter(|alias| !alias.is_empty()) {
            store.set_alias(&session_id, alias)?;
        }
        drop(store);

        self.set_agent_session(agent_id.to_string(), session_id.clone());
        Ok(session_id)
    }

    pub fn bind_resumed_session(
        &self,
        agent_id: &str,
        backend_kind: &str,
        workspace_root: Option<&str>,
        workspace_roots: &[String],
        backend_session_id: &str,
    ) -> Result<ResumeSessionBinding, String> {
        let mut store = self.session_store.lock();
        let (session_id, created_record_id) = if let Some(existing) =
            store.get_by_backend_session(backend_kind, backend_session_id)
        {
            (existing.id.clone(), None)
        } else {
            let record = store.create(backend_kind, workspace_root, workspace_roots)?;
            store.set_backend_session_id(&record.id, backend_session_id)?;
            let created_record_id = record.id.clone();
            (record.id, Some(created_record_id))
        };
        drop(store);

        self.set_agent_session(agent_id.to_string(), session_id);

        Ok(ResumeSessionBinding { created_record_id })
    }

    pub fn backend_session_id_for_agent(&self, agent_id: &str) -> Option<String> {
        let session_id = self.session_id_for_agent(agent_id)?;
        self.session_store
            .lock()
            .get(&session_id)
            .and_then(|record| record.backend_session_id.clone())
    }

    pub fn set_alias_for_agent(&self, agent_id: &str, alias: &str) -> Result<(), String> {
        let Some(session_id) = self.session_id_for_agent(agent_id) else {
            return Ok(());
        };
        self.session_store.lock().set_alias(&session_id, alias)
    }

    pub fn delete_session_record(&self, id: &str) -> Result<(), String> {
        self.session_store.lock().delete(id)
    }

    pub fn apply_session_event(&self, agent_id: &str, event: &ChatEvent) -> Result<(), String> {
        let Some(session_id) = self.session_id_for_agent(agent_id) else {
            return Ok(());
        };

        match event {
            ChatEvent::MessageAdded(ChatMessage {
                sender: MessageSender::User,
                ..
            }) => {
                self.session_store
                    .lock()
                    .increment_message_count(&session_id)?;
            }
            ChatEvent::StreamEnd(_) => {
                self.session_store
                    .lock()
                    .increment_message_count(&session_id)?;
            }
            ChatEvent::TaskUpdate(TaskList { title, .. }) => {
                let trimmed = title.trim();
                if !trimmed.is_empty() {
                    self.session_store.lock().set_alias(&session_id, trimmed)?;
                }
            }
            ChatEvent::SessionStarted(SessionStartedData {
                session_id: backend_session_id,
            }) => {
                self.session_store
                    .lock()
                    .set_backend_session_id(&session_id, backend_session_id)?;
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use parking_lot::Mutex as SyncMutex;

    use crate::stores::SessionStore;

    use super::ConversationSessionRegistry;

    fn test_registry() -> ConversationSessionRegistry {
        let dir = tempfile::tempdir().unwrap();
        let session_store =
            SessionStore::load(PathBuf::from(dir.path()).join("sessions.json")).unwrap();
        ConversationSessionRegistry::new(
            Arc::new(SyncMutex::new(session_store)),
            Arc::new(SyncMutex::new(HashMap::new())),
        )
    }

    #[test]
    fn creates_parented_session_bindings() {
        let registry = test_registry();
        let parent_id = registry
            .create_session_binding(
                "agent-1",
                "codex",
                Some("/tmp/project"),
                &[String::from("/tmp/project")],
                None,
                Some("Parent"),
            )
            .unwrap();

        let child_id = registry
            .create_session_binding(
                "agent-2",
                "codex",
                Some("/tmp/project"),
                &[String::from("/tmp/project")],
                Some("agent-1"),
                Some("Child"),
            )
            .unwrap();

        let mut store = registry.session_store.lock();
        let child = store.get(&child_id).unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(parent_id.as_str()));
        assert_eq!(child.alias.as_deref(), Some("Child"));
    }

    #[test]
    fn reuses_existing_backend_session_records_on_resume() {
        let registry = test_registry();
        let existing_id = registry
            .create_session_binding(
                "agent-1",
                "codex",
                Some("/tmp/project"),
                &[String::from("/tmp/project")],
                None,
                None,
            )
            .unwrap();
        registry
            .session_store
            .lock()
            .set_backend_session_id(&existing_id, "backend-1")
            .unwrap();

        let binding = registry
            .bind_resumed_session(
                "agent-2",
                "codex",
                Some("/tmp/project"),
                &[String::from("/tmp/project")],
                "backend-1",
            )
            .unwrap();

        assert!(binding.created_record_id.is_none());
        assert_eq!(
            registry.session_id_for_agent("agent-2"),
            Some(existing_id.to_string())
        );
    }
}
