use std::collections::HashMap;

struct AdminEntry<S> {
    session: S,
}

pub struct AdminManager<S> {
    subprocesses: HashMap<u64, AdminEntry<S>>,
    next_id: u64,
}

impl<S> AdminManager<S> {
    pub fn new() -> Self {
        Self {
            subprocesses: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn create(&mut self, session: S) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.subprocesses.insert(id, AdminEntry { session });
        id
    }

    pub fn get(&self, id: u64) -> Option<&S> {
        self.subprocesses.get(&id).map(|entry| &entry.session)
    }

    pub fn remove(&mut self, id: u64) -> Option<S> {
        self.subprocesses.remove(&id).map(|entry| entry.session)
    }

    pub fn active_ids(&self) -> Vec<u64> {
        self.subprocesses.keys().copied().collect()
    }

    pub fn drain_all(&mut self) -> Vec<S> {
        self.subprocesses
            .drain()
            .map(|(_, entry)| entry.session)
            .collect()
    }
}

impl<S> Default for AdminManager<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::AdminManager;

    #[test]
    fn create_and_remove_admin_session() {
        let mut manager = AdminManager::new();
        let id = manager.create("session");
        assert_eq!(manager.get(id), Some(&"session"));
        assert_eq!(manager.remove(id), Some("session"));
        assert!(manager.get(id).is_none());
    }
}
