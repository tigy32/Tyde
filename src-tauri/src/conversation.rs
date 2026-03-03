use std::collections::HashMap;

use crate::backend::{BackendKind, BackendSession};

struct ConversationEntry {
    session: BackendSession,
    backend_kind: BackendKind,
    workspace_roots: Vec<String>,
}

pub struct ConversationManager {
    conversations: HashMap<u64, ConversationEntry>,
    next_id: u64,
}

impl ConversationManager {
    pub fn new() -> Self {
        Self {
            conversations: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn create_conversation(
        &mut self,
        session: BackendSession,
        workspace_roots: &[String],
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.conversations.insert(
            id,
            ConversationEntry {
                backend_kind: session.kind(),
                session,
                workspace_roots: workspace_roots.to_vec(),
            },
        );
        id
    }

    pub fn get(&self, id: u64) -> Option<&BackendSession> {
        self.conversations.get(&id).map(|e| &e.session)
    }

    pub fn backend_kind(&self, id: u64) -> Option<BackendKind> {
        self.conversations.get(&id).map(|e| e.backend_kind)
    }

    pub fn workspace_roots(&self, id: u64) -> Option<&[String]> {
        self.conversations
            .get(&id)
            .map(|e| e.workspace_roots.as_slice())
    }

    pub fn remove(&mut self, id: u64) -> Option<BackendSession> {
        self.conversations.remove(&id).map(|e| e.session)
    }

    pub fn insert(&mut self, id: u64, session: BackendSession, workspace_roots: Vec<String>) {
        self.conversations.insert(
            id,
            ConversationEntry {
                backend_kind: session.kind(),
                session,
                workspace_roots,
            },
        );
    }

    pub fn active_ids(&self) -> Vec<u64> {
        self.conversations.keys().copied().collect()
    }

    pub fn drain_all(&mut self) -> Vec<BackendSession> {
        self.conversations.drain().map(|(_, e)| e.session).collect()
    }
}
