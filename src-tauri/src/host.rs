use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: String,
    pub label: String,
    pub hostname: String,
    pub is_local: bool,
    pub enabled_backends: Vec<String>,
    pub default_backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostFile {
    hosts: Vec<Host>,
}

pub struct HostStore {
    path: PathBuf,
    hosts: Vec<Host>,
}

fn default_local_host() -> Host {
    Host {
        id: "local".to_string(),
        label: "Local".to_string(),
        hostname: String::new(),
        is_local: true,
        enabled_backends: vec![
            "tycode".to_string(),
            "codex".to_string(),
            "claude".to_string(),
            "kiro".to_string(),
            "gemini".to_string(),
        ],
        default_backend: "tycode".to_string(),
    }
}

fn all_backends() -> Vec<String> {
    vec![
        "tycode".to_string(),
        "codex".to_string(),
        "claude".to_string(),
        "kiro".to_string(),
        "gemini".to_string(),
    ]
}

impl HostStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let store = Self {
                    path,
                    hosts: vec![default_local_host()],
                };
                store.save()?;
                return Ok(store);
            }
            Err(err) => {
                return Err(format!(
                    "Failed to read hosts file {}: {err:?}",
                    path.display()
                ));
            }
        };

        let file: HostFile = match serde_json::from_str(&raw) {
            Ok(file) => file,
            Err(err) => {
                tracing::error!(
                    "Corrupt hosts file {}, resetting to defaults: {err:?}",
                    path.display()
                );
                let store = Self {
                    path,
                    hosts: vec![default_local_host()],
                };
                store.save()?;
                return Ok(store);
            }
        };

        let mut hosts = file.hosts;

        if !hosts.iter().any(|h| h.id == "local") {
            hosts.insert(0, default_local_host());
        }

        // Ensure newly added backends appear in each host's enabled list.
        let known = all_backends();
        let mut migrated = false;
        for host in &mut hosts {
            for backend in &known {
                if !host.enabled_backends.contains(backend) {
                    host.enabled_backends.push(backend.clone());
                    migrated = true;
                }
            }
        }

        let store = Self { path, hosts };
        if migrated {
            store.save()?;
        }
        Ok(store)
    }

    pub fn list(&self) -> Vec<Host> {
        let mut result = self.hosts.clone();
        result.sort_by(|a, b| {
            if a.is_local {
                return std::cmp::Ordering::Less;
            }
            if b.is_local {
                return std::cmp::Ordering::Greater;
            }
            a.label.to_lowercase().cmp(&b.label.to_lowercase())
        });
        result
    }

    pub fn get(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }

    pub fn add(&mut self, label: String, hostname: String) -> Result<Host, String> {
        let host = Host {
            id: uuid::Uuid::new_v4().to_string(),
            label,
            hostname,
            is_local: false,
            enabled_backends: all_backends(),
            default_backend: "tycode".to_string(),
        };
        self.hosts.push(host.clone());
        self.save()?;
        Ok(host)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        if id == "local" {
            return Err("Cannot remove the local host".to_string());
        }
        let before = self.hosts.len();
        self.hosts.retain(|h| h.id != id);
        if self.hosts.len() == before {
            return Err(format!("Host '{id}' not found"));
        }
        self.save()
    }

    pub fn update_enabled_backends(
        &mut self,
        id: &str,
        backends: Vec<String>,
    ) -> Result<(), String> {
        let host = self
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| format!("Host '{id}' not found"))?;
        host.enabled_backends = backends;
        self.save()
    }

    pub fn update_default_backend(&mut self, id: &str, backend: String) -> Result<(), String> {
        let host = self
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| format!("Host '{id}' not found"))?;
        host.default_backend = backend;
        self.save()
    }

    pub fn update_label(&mut self, id: &str, label: String) -> Result<(), String> {
        let host = self
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| format!("Host '{id}' not found"))?;
        host.label = label;
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!("Failed to create directory {}: {err:?}", parent.display())
            })?;
        }
        let file = HostFile {
            hosts: self.hosts.clone(),
        };
        let data = serde_json::to_string_pretty(&file)
            .map_err(|err| format!("Failed to serialize hosts: {err:?}"))?;
        fs::write(&self.path, data)
            .map_err(|err| format!("Failed to write hosts to {}: {err:?}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_creates_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let store = HostStore::load(path.clone()).unwrap();
        let hosts = store.list();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].id, "local");
        assert!(path.exists());
    }

    #[test]
    fn add_and_remove_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let mut store = HostStore::load(path).unwrap();

        let host = store
            .add("Dev Server".into(), "user@dev.example.com".into())
            .unwrap();
        assert!(!host.is_local);
        assert_eq!(store.list().len(), 2);

        store.remove(&host.id).unwrap();
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn cannot_remove_local() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let mut store = HostStore::load(path).unwrap();
        assert!(store.remove("local").is_err());
    }

    #[test]
    fn update_backends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let mut store = HostStore::load(path).unwrap();

        store
            .update_enabled_backends("local", vec!["tycode".into()])
            .unwrap();
        let host = store.get("local").unwrap();
        assert_eq!(host.enabled_backends, vec!["tycode"]);

        store
            .update_default_backend("local", "claude".into())
            .unwrap();
        let host = store.get("local").unwrap();
        assert_eq!(host.default_backend, "claude");
    }

    #[test]
    fn list_sorts_local_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let mut store = HostStore::load(path).unwrap();

        store.add("Alpha".into(), "alpha.com".into()).unwrap();
        store.add("Beta".into(), "beta.com".into()).unwrap();

        let hosts = store.list();
        assert_eq!(hosts[0].id, "local");
        assert_eq!(hosts[1].label, "Alpha");
        assert_eq!(hosts[2].label, "Beta");
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");

        {
            let mut store = HostStore::load(path.clone()).unwrap();
            store
                .add("My Server".into(), "me@server.com".into())
                .unwrap();
        }

        let store = HostStore::load(path).unwrap();
        assert_eq!(store.list().len(), 2);
        assert_eq!(store.list()[1].label, "My Server");
    }
}
