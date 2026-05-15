use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{CustomAgent, CustomAgentId, ToolPolicy};
use serde::{Deserialize, Serialize};

pub const TEAM_LEAD_CUSTOM_AGENT_ID: &str = "tyde-team-lead";
pub const CODE_REVIEWER_CUSTOM_AGENT_ID: &str = "tyde-code-reviewer";
pub const FRONTEND_ENGINEER_CUSTOM_AGENT_ID: &str = "tyde-frontend-engineer";
pub const BACKEND_ENGINEER_CUSTOM_AGENT_ID: &str = "tyde-backend-engineer";
pub const TEST_QA_ENGINEER_CUSTOM_AGENT_ID: &str = "tyde-test-qa-engineer";

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, CustomAgent>,
}

#[derive(Debug)]
pub struct CustomAgentStore {
    path: PathBuf,
}

impl CustomAgentStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let mut records = Self::read_from_disk(&path)?;
        let mut changed = false;
        for custom_agent in builtin_team_custom_agents() {
            if !records.contains_key(&custom_agent.id.0) {
                validate_custom_agent(&custom_agent)?;
                records.insert(custom_agent.id.0.clone(), custom_agent);
                changed = true;
            }
        }
        let store = Self { path };
        if changed {
            store.save(&records)?;
        }
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_CUSTOM_AGENTS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("custom_agents.json"))
    }

    pub fn list(&self) -> Result<Vec<CustomAgent>, String> {
        let mut agents = Self::read_from_disk(&self.path)?
            .into_values()
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(agents)
    }

    pub fn get(&self, id: &CustomAgentId) -> Option<CustomAgent> {
        Self::read_from_disk(&self.path)
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn upsert(&self, custom_agent: CustomAgent) -> Result<CustomAgent, String> {
        validate_custom_agent(&custom_agent)?;
        let mut records = Self::read_from_disk(&self.path)?;
        records.insert(custom_agent.id.0.clone(), custom_agent.clone());
        self.save(&records)?;
        Ok(custom_agent)
    }

    pub fn delete(&self, id: &CustomAgentId) -> Result<CustomAgentId, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        if records.remove(&id.0).is_none() {
            return Err(format!("cannot delete missing custom agent {}", id));
        }
        self.save(&records)?;
        Ok(id.clone())
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<String, CustomAgent>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let records = serde_json::from_str::<StoreFile>(&contents)
                    .map(|store| store.records)
                    .map_err(|err| {
                        format!(
                            "Failed to parse custom agent store {}: {err}",
                            path.display()
                        )
                    })?;
                for custom_agent in records.values() {
                    validate_custom_agent(custom_agent).map_err(|err| {
                        format!("Invalid custom agent store {}: {err}", path.display())
                    })?;
                }
                Ok(records)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read custom agent store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, records: &HashMap<String, CustomAgent>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
        .map_err(|err| format!("Failed to serialize custom agent store: {err}"))?;

        let parent = self.path.parent().ok_or_else(|| {
            format!(
                "Custom agent store path has no parent: {}",
                self.path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create custom agent store directory: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp custom agent store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp custom agent store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp custom agent store file: {err}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "Failed to atomically replace custom agent store {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

pub fn builtin_team_custom_agents() -> Vec<CustomAgent> {
    vec![
        CustomAgent {
            id: CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned()),
            name: "Team Lead".to_owned(),
            description: "Plans work, coordinates teammates, and keeps scope tight.".to_owned(),
            instructions: Some(
                "Act as a pragmatic team lead. Break work into clear tasks, coordinate other agents, surface risks early, and keep the implementation focused on the requested outcome."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(CODE_REVIEWER_CUSTOM_AGENT_ID.to_owned()),
            name: "Code Reviewer".to_owned(),
            description: "Reviews correctness, maintainability, tests, and architecture.".to_owned(),
            instructions: Some(
                "Act as a code reviewer. Look for correctness bugs, missing tests, broken invariants, architecture drift, and maintainability risks. Be specific and actionable."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(FRONTEND_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Frontend Engineer".to_owned(),
            description: "Builds UI behavior, state projection, and interaction polish.".to_owned(),
            instructions: Some(
                "Act as a frontend engineer. Focus on typed UI state, reactive rendering, accessibility, and user-visible behavior. Avoid frontend-owned domain semantics."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(BACKEND_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Backend Engineer".to_owned(),
            description: "Owns server behavior, persistence, validation, and protocol flow.".to_owned(),
            instructions: Some(
                "Act as a backend engineer. Keep behavior server-owned, validate invariants loudly, preserve typed protocol flow, and avoid silent fallbacks."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
        CustomAgent {
            id: CustomAgentId(TEST_QA_ENGINEER_CUSTOM_AGENT_ID.to_owned()),
            name: "Test / QA Engineer".to_owned(),
            description: "Adds focused tests and verifies observable behavior.".to_owned(),
            instructions: Some(
                "Act as a test and QA engineer. Start from observable behavior, add focused coverage, reproduce failures before fixing them, and report exact checks."
                    .to_owned(),
            ),
            skill_ids: Vec::new(),
            mcp_server_ids: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        },
    ]
}

fn validate_custom_agent(custom_agent: &CustomAgent) -> Result<(), String> {
    if custom_agent.id.0.trim().is_empty() {
        return Err("custom agent id must not be empty".to_string());
    }
    if custom_agent.name.trim().is_empty() {
        return Err(format!(
            "custom agent {} name must not be empty",
            custom_agent.id
        ));
    }
    if custom_agent.description.trim().is_empty() {
        return Err(format!(
            "custom agent {} description must not be empty",
            custom_agent.id
        ));
    }
    if custom_agent
        .instructions
        .as_ref()
        .is_some_and(|instructions| instructions.trim().is_empty())
    {
        return Err(format!(
            "custom agent {} instructions must not be blank when provided",
            custom_agent.id
        ));
    }

    validate_id_list(
        "skill_ids",
        &custom_agent.id.0,
        custom_agent
            .skill_ids
            .iter()
            .map(|id| id.0.as_str())
            .collect(),
    )?;
    validate_id_list(
        "mcp_server_ids",
        &custom_agent.id.0,
        custom_agent
            .mcp_server_ids
            .iter()
            .map(|id| id.0.as_str())
            .collect(),
    )?;

    match &custom_agent.tool_policy {
        ToolPolicy::Unrestricted => {}
        ToolPolicy::AllowList { tools } | ToolPolicy::DenyList { tools } => {
            if tools.is_empty() {
                return Err(format!(
                    "custom agent {} tool policy must not have an empty tools list",
                    custom_agent.id
                ));
            }
            let mut seen = std::collections::HashSet::new();
            for tool in tools {
                let trimmed = tool.trim();
                if trimmed.is_empty() {
                    return Err(format!(
                        "custom agent {} tool policy contains a blank tool name",
                        custom_agent.id
                    ));
                }
                if !seen.insert(trimmed.to_string()) {
                    return Err(format!(
                        "custom agent {} tool policy contains duplicate tool '{}'",
                        custom_agent.id, trimmed
                    ));
                }
            }
        }
    }

    Ok(())
}

fn validate_id_list(label: &str, owner_id: &str, values: Vec<&str>) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "{label} for custom agent {owner_id} must not contain blank ids"
            ));
        }
        if !seen.insert(trimmed.to_string()) {
            return Err(format!(
                "{label} for custom agent {owner_id} contains duplicate id '{}'",
                trimmed
            ));
        }
    }
    Ok(())
}
