use std::collections::HashMap;

use crate::backend::BackendSession;

struct AdminEntry {
    session: BackendSession,
}

pub struct AdminManager {
    subprocesses: HashMap<u64, AdminEntry>,
    next_id: u64,
}

impl AdminManager {
    pub fn new() -> Self {
        Self {
            subprocesses: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn create(&mut self, session: BackendSession) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.subprocesses.insert(id, AdminEntry { session });
        id
    }

    pub fn get(&self, id: u64) -> Option<&BackendSession> {
        self.subprocesses.get(&id).map(|entry| &entry.session)
    }

    pub fn remove(&mut self, id: u64) -> Option<BackendSession> {
        self.subprocesses.remove(&id).map(|entry| entry.session)
    }

    pub fn active_ids(&self) -> Vec<u64> {
        self.subprocesses.keys().copied().collect()
    }

    pub fn drain_all(&mut self) -> Vec<BackendSession> {
        self.subprocesses
            .drain()
            .map(|(_, entry)| entry.session)
            .collect()
    }
}
