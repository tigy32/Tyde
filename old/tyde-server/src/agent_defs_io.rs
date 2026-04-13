use std::collections::HashMap;
use std::path::PathBuf;

use crate::ToolPolicy;
use serde::{Deserialize, Serialize};
use tokio::fs;

/// On-disk schema — scope is NOT stored, it is inferred from file location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_prompt: Option<String>,
    /// Skill names from `~/.tyde/skills/`, e.g. `["code-review", "test-writer"]`.
    /// Resolved and injected into the backend at launch time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<AgentMcpServer>,
    #[serde(default)]
    pub tool_policy: ToolPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_backend: Option<String>,
    #[serde(default)]
    pub include_agent_control: bool,
    #[serde(default)]
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMcpServer {
    pub name: String,
    pub transport: AgentMcpTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentMcpTransport {
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

/// What the frontend receives — adds scope to the definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinitionEntry {
    #[serde(flatten)]
    pub definition: AgentDefinition,
    pub scope: String,
}

const RESERVED_MCP_NAMES: &[&str] = &["tyde_agent_control", "tyde_driver"];

fn builtin_definitions() -> Vec<AgentDefinition> {
    vec![AgentDefinition {
        id: "bridge".into(),
        name: "Bridge".into(),
        description: "Orchestrator that coordinates work between human and agents".into(),
        instructions: None,
        bootstrap_prompt: None,
        skill_names: vec![],
        mcp_servers: vec![],
        tool_policy: ToolPolicy::Unrestricted,
        default_backend: None,
        include_agent_control: true,
        builtin: true,
    }]
}

fn resolve_global_agents_dir() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home).join(".tyde").join("agents"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(profile).join(".tyde").join("agents"));
    }
    Err("Could not determine home directory for agent definitions".to_string())
}

fn resolve_project_agents_dir(workspace_path: &str) -> PathBuf {
    PathBuf::from(workspace_path).join(".tyde").join("agents")
}

async fn read_definitions_from_dir(dir: &PathBuf, scope: &str) -> Vec<AgentDefinitionEntry> {
    let mut entries = Vec::new();

    let mut reader = match fs::read_dir(dir).await {
        Ok(reader) => reader,
        Err(_) => return entries,
    };

    while let Ok(Some(entry)) = reader.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            match fs::read_to_string(&path).await {
                Ok(content) => match serde_json::from_str::<AgentDefinition>(&content) {
                    Ok(def) => {
                        entries.push(AgentDefinitionEntry {
                            definition: def,
                            scope: scope.to_string(),
                        });
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse agent definition {}: {err}",
                            path.display()
                        );
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        "Failed to read agent definition file {}: {err}",
                        path.display()
                    );
                }
            }
        }
    }

    entries
}

pub async fn list_agent_definitions(
    workspace_path: Option<String>,
) -> Result<Vec<AgentDefinitionEntry>, String> {
    let mut all: Vec<AgentDefinitionEntry> = builtin_definitions()
        .into_iter()
        .map(|def| AgentDefinitionEntry {
            definition: def,
            scope: "builtin".to_string(),
        })
        .collect();

    let global_dir = resolve_global_agents_dir()?;
    let global_entries = read_definitions_from_dir(&global_dir, "global").await;
    for entry in global_entries {
        all.retain(|e| e.definition.id != entry.definition.id);
        all.push(entry);
    }

    if let Some(wp) = workspace_path {
        if !wp.trim().is_empty() {
            let project_dir = resolve_project_agents_dir(&wp);
            let project_entries = read_definitions_from_dir(&project_dir, "project").await;
            for entry in project_entries {
                all.retain(|e| e.definition.id != entry.definition.id);
                all.push(entry);
            }
        }
    }

    Ok(all)
}

pub async fn save_agent_definition(
    definition_json: &str,
    scope: &str,
    workspace_path: Option<String>,
) -> Result<(), String> {
    let def: AgentDefinition = serde_json::from_str(definition_json)
        .map_err(|err| format!("Invalid agent definition JSON: {err}"))?;

    if def.id.trim().is_empty() {
        return Err("Agent definition id must be non-empty".to_string());
    }
    if def.name.trim().is_empty() {
        return Err("Agent definition name must be non-empty".to_string());
    }

    for server in &def.mcp_servers {
        if RESERVED_MCP_NAMES.contains(&server.name.as_str()) {
            return Err(format!(
                "MCP server name '{}' is reserved and cannot be used in agent definitions",
                server.name
            ));
        }
    }

    let dir = match scope {
        "global" => resolve_global_agents_dir()?,
        "project" => {
            let wp = workspace_path.ok_or_else(|| {
                "workspace_path is required for project-scoped agent definitions".to_string()
            })?;
            resolve_project_agents_dir(&wp)
        }
        other => return Err(format!("Invalid scope: {other}")),
    };

    fs::create_dir_all(&dir).await.map_err(|err| {
        format!(
            "Failed to create agent definitions directory {}: {err}",
            dir.display()
        )
    })?;

    let file_path = dir.join(format!("{}.json", def.id));
    let content = serde_json::to_string_pretty(&def)
        .map_err(|err| format!("Failed to serialize agent definition: {err}"))?;

    fs::write(&file_path, content).await.map_err(|err| {
        format!(
            "Failed to write agent definition file {}: {err}",
            file_path.display()
        )
    })?;

    Ok(())
}

pub async fn delete_agent_definition(
    id: &str,
    scope: &str,
    workspace_path: Option<String>,
) -> Result<(), String> {
    if scope == "builtin" {
        return Err("Cannot delete builtin agent definitions".to_string());
    }

    let dir = match scope {
        "global" => resolve_global_agents_dir()?,
        "project" => {
            let wp = workspace_path.ok_or_else(|| {
                "workspace_path is required for project-scoped agent definitions".to_string()
            })?;
            resolve_project_agents_dir(&wp)
        }
        other => return Err(format!("Invalid scope: {other}")),
    };

    let file_path = dir.join(format!("{id}.json"));
    if !file_path.exists() {
        return Err(format!(
            "Agent definition file not found: {}",
            file_path.display()
        ));
    }

    fs::remove_file(&file_path).await.map_err(|err| {
        format!(
            "Failed to delete agent definition file {}: {err}",
            file_path.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as sync_fs;

    #[tokio::test]
    async fn test_builtin_definitions_present() {
        // Verify builtins contain the Bridge definition.
        let builtins = builtin_definitions();
        let bridge = builtins.iter().find(|d| d.id == "bridge").unwrap();
        assert_eq!(bridge.name, "Bridge");
        assert!(bridge.include_agent_control);

        // Also verify list_agent_definitions always returns exactly one "bridge" entry
        // (whether builtin or overridden by global/project scope).
        let entries = list_agent_definitions(None).await.unwrap();
        let bridge_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.definition.id == "bridge")
            .collect();
        assert_eq!(bridge_entries.len(), 1);
    }

    #[tokio::test]
    async fn test_save_and_list_agent_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let def_json = r#"{
            "id": "test-agent",
            "name": "Test Agent",
            "description": "A test agent",
            "instructions": "Do things",
            "mcp_servers": [],
            "tool_policy": { "mode": "Unrestricted" },
            "include_agent_control": false,
            "builtin": false
        }"#;

        save_agent_definition(def_json, "project", Some(project_path.clone()))
            .await
            .unwrap();

        let entries = list_agent_definitions(Some(project_path.clone()))
            .await
            .unwrap();
        let test: Vec<_> = entries
            .iter()
            .filter(|e| e.definition.id == "test-agent")
            .collect();
        assert_eq!(test.len(), 1);
        assert_eq!(test[0].definition.name, "Test Agent");
        assert_eq!(test[0].scope, "project");
    }

    #[tokio::test]
    async fn test_save_and_delete_agent_definition() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let def_json = r#"{
            "id": "del-test",
            "name": "Delete Test",
            "description": "",
            "mcp_servers": [],
            "tool_policy": { "mode": "Unrestricted" },
            "include_agent_control": false,
            "builtin": false
        }"#;

        save_agent_definition(def_json, "project", Some(project_path.clone()))
            .await
            .unwrap();

        let file_path = tmp
            .path()
            .join(".tyde")
            .join("agents")
            .join("del-test.json");
        assert!(file_path.exists());

        delete_agent_definition("del-test", "project", Some(project_path.clone()))
            .await
            .unwrap();

        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn test_project_overrides_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let bridge_override = r#"{
            "id": "bridge",
            "name": "Custom Bridge",
            "description": "Overridden bridge",
            "instructions": "Custom instructions",
            "mcp_servers": [],
            "tool_policy": { "mode": "Unrestricted" },
            "include_agent_control": true,
            "builtin": false
        }"#;

        save_agent_definition(bridge_override, "project", Some(project_path.clone()))
            .await
            .unwrap();

        let entries = list_agent_definitions(Some(project_path)).await.unwrap();
        let bridge: Vec<_> = entries
            .iter()
            .filter(|e| e.definition.id == "bridge")
            .collect();
        assert_eq!(bridge.len(), 1);
        assert_eq!(bridge[0].definition.name, "Custom Bridge");
        assert_eq!(bridge[0].scope, "project");
    }

    #[tokio::test]
    async fn test_reserved_mcp_name_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let def_json = r#"{
            "id": "bad-agent",
            "name": "Bad Agent",
            "description": "",
            "mcp_servers": [
                { "name": "tyde_agent_control", "transport": { "type": "http", "url": "http://localhost" } }
            ],
            "tool_policy": { "mode": "Unrestricted" },
            "include_agent_control": false,
            "builtin": false
        }"#;

        let result = save_agent_definition(def_json, "project", Some(project_path)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("reserved"));
    }

    #[tokio::test]
    async fn test_cannot_delete_builtin() {
        let result = delete_agent_definition("bridge", "builtin", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("builtin"));
    }

    #[tokio::test]
    async fn test_global_overrides_builtin() {
        let global_dir = resolve_global_agents_dir().unwrap();
        sync_fs::create_dir_all(&global_dir).ok();

        let def_json = r#"{
            "id": "bridge",
            "name": "Global Bridge",
            "description": "Globally overridden bridge",
            "mcp_servers": [],
            "tool_policy": { "mode": "Unrestricted" },
            "include_agent_control": true,
            "builtin": false
        }"#;
        sync_fs::write(global_dir.join("bridge.json"), def_json).unwrap();

        let entries = list_agent_definitions(None).await.unwrap();
        let bridge: Vec<_> = entries
            .iter()
            .filter(|e| e.definition.id == "bridge")
            .collect();
        assert_eq!(bridge.len(), 1);
        assert_eq!(bridge[0].definition.name, "Global Bridge");
        assert_eq!(bridge[0].scope, "global");

        // Cleanup
        sync_fs::remove_file(global_dir.join("bridge.json")).ok();
    }

    #[tokio::test]
    async fn test_tool_policy_deny_list() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let def_json = r#"{
            "id": "restricted-agent",
            "name": "Restricted Agent",
            "description": "",
            "mcp_servers": [],
            "tool_policy": { "mode": "DenyList", "tools": ["tyde_spawn_agent"] },
            "include_agent_control": false,
            "builtin": false
        }"#;

        save_agent_definition(def_json, "project", Some(project_path.clone()))
            .await
            .unwrap();

        let entries = list_agent_definitions(Some(project_path)).await.unwrap();
        let agent: Vec<_> = entries
            .iter()
            .filter(|e| e.definition.id == "restricted-agent")
            .collect();
        assert_eq!(agent.len(), 1);
        match &agent[0].definition.tool_policy {
            ToolPolicy::DenyList(tools) => {
                assert_eq!(tools, &["tyde_spawn_agent"]);
            }
            _ => panic!("Expected DenyList"),
        }
    }
}
