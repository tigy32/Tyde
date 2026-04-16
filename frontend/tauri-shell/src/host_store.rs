use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use host_config::{
    ConfiguredHost, ConfiguredHostStore, HostTransportConfig, LOCAL_HOST_ID,
    UpsertConfiguredHostRequest,
};

const HOST_STORE_PATH_ENV: &str = "TYDE_CONFIGURED_HOST_STORE_PATH";

#[derive(Debug, Clone)]
pub struct HostStore {
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    hosts: Vec<ConfiguredHost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_host_id: Option<String>,
}

impl HostStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let _ = Self::read_from_disk(&path)?;
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var(HOST_STORE_PATH_ENV) {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home)
            .join(".tyde")
            .join("configured_hosts.json"))
    }

    pub fn list(&self) -> Result<ConfiguredHostStore, String> {
        Self::read_from_disk(&self.path)
    }

    pub fn get(&self, host_id: &str) -> Result<Option<ConfiguredHost>, String> {
        let store = Self::read_from_disk(&self.path)?;
        Ok(store.hosts.into_iter().find(|host| host.id == host_id))
    }

    pub fn upsert(
        &self,
        request: UpsertConfiguredHostRequest,
    ) -> Result<ConfiguredHostStore, String> {
        self.read_modify_write(|store| {
            let label = validate_label(&request.label)?;
            validate_transport(&request.transport)?;

            let id = request
                .id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string());

            if id == LOCAL_HOST_ID
                && !matches!(request.transport, HostTransportConfig::LocalEmbedded)
            {
                return Err("local host transport must remain local_embedded".to_string());
            }

            let host = ConfiguredHost {
                id: id.clone(),
                label,
                transport: request.transport,
                auto_connect: request.auto_connect,
            };

            if let Some(existing) = store.hosts.iter_mut().find(|existing| existing.id == id) {
                *existing = host;
            } else {
                store.hosts.push(host);
            }

            normalize_store(store);
            Ok(())
        })
    }

    pub fn remove(&self, host_id: &str) -> Result<ConfiguredHostStore, String> {
        if host_id == LOCAL_HOST_ID {
            return Err("cannot remove local host".to_string());
        }

        self.read_modify_write(|store| {
            let original_len = store.hosts.len();
            store.hosts.retain(|host| host.id != host_id);
            if store.hosts.len() == original_len {
                return Err(format!("configured host '{}' not found", host_id));
            }
            normalize_store(store);
            Ok(())
        })
    }

    pub fn set_selected_host(
        &self,
        host_id: Option<String>,
    ) -> Result<ConfiguredHostStore, String> {
        self.read_modify_write(|store| {
            if let Some(ref host_id) = host_id
                && !store.hosts.iter().any(|host| &host.id == host_id)
            {
                return Err(format!("configured host '{}' not found", host_id));
            }
            store.selected_host_id = host_id;
            normalize_store(store);
            Ok(())
        })
    }

    fn read_modify_write<F>(&self, modify: F) -> Result<ConfiguredHostStore, String>
    where
        F: FnOnce(&mut ConfiguredHostStore) -> Result<(), String>,
    {
        let mut store = Self::read_from_disk(&self.path)?;
        modify(&mut store)?;
        Self::save(&self.path, &store)?;
        Ok(store)
    }

    fn read_from_disk(path: &Path) -> Result<ConfiguredHostStore, String> {
        let store = match std::fs::read_to_string(path) {
            Ok(contents) => {
                let file = serde_json::from_str::<StoreFile>(&contents).map_err(|err| {
                    format!("Failed to parse host store {}: {err}", path.display())
                })?;
                ConfiguredHostStore {
                    hosts: file.hosts,
                    selected_host_id: file.selected_host_id,
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ConfiguredHostStore {
                hosts: Vec::new(),
                selected_host_id: None,
            },
            Err(err) => {
                return Err(format!(
                    "Failed to read host store {}: {err}",
                    path.display()
                ));
            }
        };

        let mut normalized = store;
        normalize_store(&mut normalized);
        Ok(normalized)
    }

    fn save(path: &Path, store: &ConfiguredHostStore) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            hosts: store.hosts.clone(),
            selected_host_id: store.selected_host_id.clone(),
        })
        .map_err(|err| format!("Failed to serialize host store: {err}"))?;

        let parent = path
            .parent()
            .ok_or_else(|| format!("Host store path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create host store directory: {err}"))?;

        let tmp_path = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("configured_hosts.json")
        ));
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp host store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp host store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp host store file: {err}"))?;
        std::fs::rename(&tmp_path, path).map_err(|err| {
            format!(
                "Failed to atomically replace host store {}: {err}",
                path.display()
            )
        })?;
        Ok(())
    }
}

fn normalize_store(store: &mut ConfiguredHostStore) {
    let mut local_seen = false;
    store.hosts.retain(|host| {
        if host.id == LOCAL_HOST_ID {
            if local_seen {
                return false;
            }
            local_seen = true;
        }
        true
    });

    if let Some(local) = store.hosts.iter_mut().find(|host| host.id == LOCAL_HOST_ID) {
        local.transport = HostTransportConfig::LocalEmbedded;
        if local.label.trim().is_empty() {
            local.label = "Local".to_string();
        }
    } else {
        store.hosts.push(default_local_host());
    }

    store
        .hosts
        .sort_by(|left, right| match (left.id.as_str(), right.id.as_str()) {
            (LOCAL_HOST_ID, LOCAL_HOST_ID) => std::cmp::Ordering::Equal,
            (LOCAL_HOST_ID, _) => std::cmp::Ordering::Less,
            (_, LOCAL_HOST_ID) => std::cmp::Ordering::Greater,
            _ => left.label.to_lowercase().cmp(&right.label.to_lowercase()),
        });

    if store
        .selected_host_id
        .as_ref()
        .is_none_or(|selected| !store.hosts.iter().any(|host| &host.id == selected))
    {
        store.selected_host_id = Some(LOCAL_HOST_ID.to_string());
    }
}

fn default_local_host() -> ConfiguredHost {
    ConfiguredHost {
        id: LOCAL_HOST_ID.to_string(),
        label: "Local".to_string(),
        transport: HostTransportConfig::LocalEmbedded,
        auto_connect: true,
    }
}

fn validate_label(label: &str) -> Result<String, String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err("host label must not be empty".to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_transport(transport: &HostTransportConfig) -> Result<(), String> {
    match transport {
        HostTransportConfig::LocalEmbedded => Ok(()),
        HostTransportConfig::SshStdio {
            ssh_destination,
            remote_command,
        } => {
            if ssh_destination.trim().is_empty() {
                return Err("ssh_destination must not be empty".to_string());
            }
            if remote_command
                .as_ref()
                .is_some_and(|command| command.trim().is_empty())
            {
                return Err("remote_command must not be blank when provided".to_string());
            }
            Ok(())
        }
    }
}
