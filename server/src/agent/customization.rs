use std::collections::HashMap;

use protocol::{
    BackendKind, CustomAgentId, McpServerConfig, McpServerId, McpTransportConfig, ProjectId,
    SkillId, ToolPolicy,
};

use crate::backend::{StartupMcpServer, StartupMcpTransport};
use crate::store::custom_agents::CustomAgentStore;
use crate::store::mcp_servers::{McpServerStore, RESERVED_MCP_SERVER_NAMES};
use crate::store::skills::SkillStore;
use crate::store::steering::SteeringStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub name: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpawnConfig {
    pub instructions: Option<String>,
    pub steering_body: String,
    pub skills: Vec<ResolvedSkill>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub tool_policy: ToolPolicy,
}

impl Default for ResolvedSpawnConfig {
    fn default() -> Self {
        Self {
            instructions: None,
            steering_body: String::new(),
            skills: Vec::new(),
            mcp_servers: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
        }
    }
}

pub(crate) fn resolve_spawn_config(
    backend_kind: BackendKind,
    project_id: Option<&ProjectId>,
    custom_agent_id: Option<&CustomAgentId>,
    built_in_mcp_servers: &[StartupMcpServer],
    custom_agent_store: &CustomAgentStore,
    mcp_server_store: &McpServerStore,
    steering_store: &SteeringStore,
    skill_store: &SkillStore,
) -> Result<ResolvedSpawnConfig, String> {
    let mut mcp_servers = built_in_mcp_servers
        .iter()
        .map(startup_mcp_server_to_protocol)
        .collect::<Vec<_>>();
    let mut mcp_names = mcp_servers
        .iter()
        .map(|server| (server.name.clone(), server.id.clone()))
        .collect::<HashMap<_, _>>();

    let mut instructions = None;
    let mut skills = Vec::new();
    let mut tool_policy = ToolPolicy::Unrestricted;

    if let Some(custom_agent_id) = custom_agent_id {
        let custom_agent = custom_agent_store
            .get(custom_agent_id)
            .ok_or_else(|| format!("cannot resolve missing custom agent {}", custom_agent_id))?;
        instructions = custom_agent.instructions.clone();
        tool_policy = custom_agent.tool_policy.clone();

        for skill_id in &custom_agent.skill_ids {
            skills.push(resolve_skill(skill_store, skill_id)?);
        }

        for mcp_server_id in &custom_agent.mcp_server_ids {
            let mcp_server = mcp_server_store.get(mcp_server_id).ok_or_else(|| {
                format!(
                    "custom agent {} references missing MCP server {}",
                    custom_agent.id, mcp_server_id
                )
            })?;
            let name = mcp_server.name.clone();
            if RESERVED_MCP_SERVER_NAMES.contains(&name.as_str()) {
                return Err(format!(
                    "custom agent {} references reserved MCP server name '{}'",
                    custom_agent.id, name
                ));
            }
            if let Some(existing_id) = mcp_names.get(&name) {
                return Err(format!(
                    "custom agent {} MCP server '{}' collides with existing server {}",
                    custom_agent.id, name, existing_id
                ));
            }
            mcp_names.insert(name, mcp_server.id.clone());
            mcp_servers.push(mcp_server);
        }
    }

    match &tool_policy {
        ToolPolicy::Unrestricted => {}
        ToolPolicy::AllowList { .. } | ToolPolicy::DenyList { .. } => {
            if backend_kind != BackendKind::Claude {
                return Err(format!(
                    "backend {:?} does not support tool policy {:?}",
                    backend_kind, tool_policy
                ));
            }
        }
    }

    let steering_body = resolve_steering_body(steering_store, project_id)?;

    Ok(ResolvedSpawnConfig {
        instructions,
        steering_body,
        skills,
        mcp_servers,
        tool_policy,
    })
}

pub(crate) fn protocol_mcp_servers_to_startup(
    mcp_servers: &[McpServerConfig],
) -> Vec<StartupMcpServer> {
    mcp_servers
        .iter()
        .map(|server| StartupMcpServer {
            name: server.name.clone(),
            transport: match &server.transport {
                McpTransportConfig::Http {
                    url,
                    headers,
                    bearer_token_env_var,
                } => StartupMcpTransport::Http {
                    url: url.clone(),
                    headers: headers.clone(),
                    bearer_token_env_var: bearer_token_env_var.clone(),
                },
                McpTransportConfig::Stdio { command, args, env } => StartupMcpTransport::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.clone(),
                },
            },
        })
        .collect()
}

fn startup_mcp_server_to_protocol(server: &StartupMcpServer) -> McpServerConfig {
    McpServerConfig {
        id: McpServerId(format!("builtin:{}", server.name)),
        name: server.name.clone(),
        transport: match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => McpTransportConfig::Http {
                url: url.clone(),
                headers: headers.clone(),
                bearer_token_env_var: bearer_token_env_var.clone(),
            },
            StartupMcpTransport::Stdio { command, args, env } => McpTransportConfig::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
            },
        },
    }
}

fn resolve_skill(skill_store: &SkillStore, skill_id: &SkillId) -> Result<ResolvedSkill, String> {
    let skill = skill_store
        .get(skill_id)
        .ok_or_else(|| format!("cannot resolve missing skill {}", skill_id))?;
    let body = skill_store.load_body(skill_id)?;
    Ok(ResolvedSkill {
        name: skill.name,
        body,
    })
}

fn resolve_steering_body(
    steering_store: &SteeringStore,
    project_id: Option<&ProjectId>,
) -> Result<String, String> {
    let mut entries = steering_store
        .list()?
        .into_iter()
        .filter(|steering| match &steering.scope {
            protocol::SteeringScope::Host => true,
            protocol::SteeringScope::Project(candidate) => project_id == Some(candidate),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.title
            .cmp(&right.title)
            .then(left.id.0.cmp(&right.id.0))
    });
    Ok(entries
        .into_iter()
        .map(|entry| entry.content)
        .collect::<Vec<_>>()
        .join("\n\n"))
}
