use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{McpServerConfig, McpServerId, McpTransportConfig};
use serde::{Deserialize, Serialize};

pub const RESERVED_MCP_SERVER_NAMES: [&str; 2] = ["tyde-debug", "tyde-agent-control"];

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, McpServerConfig>,
}

#[derive(Debug)]
pub struct McpServerStore {
    path: PathBuf,
}

impl McpServerStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let _ = Self::read_from_disk(&path)?;
        Ok(Self { path })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_MCP_SERVERS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("mcp_servers.json"))
    }

    pub fn list(&self) -> Result<Vec<McpServerConfig>, String> {
        let mut servers = Self::read_from_disk(&self.path)?
            .into_values()
            .collect::<Vec<_>>();
        servers.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(servers)
    }

    pub fn get(&self, id: &McpServerId) -> Option<McpServerConfig> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn upsert(&self, mcp_server: McpServerConfig) -> Result<McpServerConfig, String> {
        validate_mcp_server(&mcp_server)?;
        let mut records = Self::read_from_disk(&self.path)?;
        if let Some(existing) = records
            .values()
            .find(|existing| existing.name == mcp_server.name && existing.id != mcp_server.id)
        {
            return Err(format!(
                "cannot upsert MCP server {} with duplicate name '{}' already used by {}",
                mcp_server.id, mcp_server.name, existing.id
            ));
        }
        records.insert(mcp_server.id.0.clone(), mcp_server.clone());
        self.save(&records)?;
        Ok(mcp_server)
    }

    pub fn delete(&self, id: &McpServerId) -> Result<McpServerId, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        if records.remove(&id.0).is_none() {
            return Err(format!("cannot delete missing MCP server {}", id));
        }
        self.save(&records)?;
        Ok(id.clone())
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, McpServerConfig>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let records = serde_json::from_str::<StoreFile>(&contents)
                    .map(|store| store.records)
                    .map_err(|err| {
                        format!("Failed to parse MCP server store {}: {err}", path.display())
                    })?;
                let mut names = HashMap::<String, String>::new();
                for server in records.values() {
                    validate_mcp_server(server).map_err(|err| {
                        format!("Invalid MCP server store {}: {err}", path.display())
                    })?;
                    if let Some(previous_id) =
                        names.insert(server.name.clone(), server.id.0.clone())
                    {
                        return Err(format!(
                            "Invalid MCP server store {}: duplicate name '{}' for {} and {}",
                            path.display(),
                            server.name,
                            previous_id,
                            server.id
                        ));
                    }
                }
                Ok(records)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read MCP server store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, records: &HashMap<String, McpServerConfig>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
        .map_err(|err| format!("Failed to serialize MCP server store: {err}"))?;

        let parent = self.path.parent().ok_or_else(|| {
            format!(
                "MCP server store path has no parent: {}",
                self.path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create MCP server store directory: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp MCP server store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp MCP server store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp MCP server store file: {err}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "Failed to atomically replace MCP server store {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

fn validate_mcp_server(mcp_server: &McpServerConfig) -> Result<(), String> {
    if mcp_server.id.0.trim().is_empty() {
        return Err("MCP server id must not be empty".to_string());
    }
    let name = mcp_server.name.trim();
    if name.is_empty() {
        return Err(format!(
            "MCP server {} name must not be empty",
            mcp_server.id
        ));
    }
    match &mcp_server.transport {
        McpTransportConfig::Http {
            url,
            headers,
            bearer_token_env_var,
        } => {
            if url.trim().is_empty() {
                return Err(format!(
                    "MCP server {} HTTP url must not be empty",
                    mcp_server.id
                ));
            }
            for key in headers.keys() {
                if key.trim().is_empty() {
                    return Err(format!(
                        "MCP server {} HTTP headers must not contain blank keys",
                        mcp_server.id
                    ));
                }
            }
            if bearer_token_env_var
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(format!(
                    "MCP server {} bearer_token_env_var must not be blank when provided",
                    mcp_server.id
                ));
            }
        }
        McpTransportConfig::Stdio {
            command,
            args: _,
            env,
        } => {
            if command.trim().is_empty() {
                return Err(format!(
                    "MCP server {} stdio command must not be empty",
                    mcp_server.id
                ));
            }
            for key in env.keys() {
                if key.trim().is_empty() {
                    return Err(format!(
                        "MCP server {} stdio env must not contain blank keys",
                        mcp_server.id
                    ));
                }
            }
        }
    }

    Ok(())
}
