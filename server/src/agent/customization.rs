use std::collections::HashMap;

use protocol::{
    BackendAccessMode, BackendKind, CustomAgent, CustomAgentId, McpServerConfig, McpServerId,
    McpTransportConfig, ProjectId, SkillId, ToolPolicy,
};

use crate::backend::{StartupMcpServer, StartupMcpTransport};
use crate::store::custom_agents::CustomAgentStore;
use crate::store::mcp_servers::{McpServerStore, RESERVED_MCP_SERVER_NAMES};
use crate::store::skills::SkillStore;
use crate::store::steering::SteeringStore;

use crate::backend::customization::SkillDelivery;
pub use crate::backend::customization::{ResolvedSkill, SkillSelection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpawnConfig {
    policy: SpawnConfigPolicy,
    pub instructions: Option<String>,
    pub steering_body: String,
    /// Steering Tyde injects itself, kept apart from the user's steering
    /// records because it is a property of the session Tyde built, not
    /// something the user wrote or can see in Settings.
    ///
    /// Derived by the resolver from the same MCP list used to start the session.
    pub builtin_steering: String,
    pub skills: Vec<ResolvedSkill>,
    pub skill_selection: SkillSelection,
    pub skill_delivery: SkillDelivery,
    pub mcp_servers: Vec<McpServerConfig>,
    pub tool_policy: ToolPolicy,
    pub access_mode: BackendAccessMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnConfigPolicy {
    User,
    Reviewer,
    InferenceOnly,
    FailedStartup,
    BackendOnly,
}

impl ResolvedSpawnConfig {
    fn empty(policy: SpawnConfigPolicy) -> Self {
        Self {
            policy,
            instructions: None,
            steering_body: String::new(),
            builtin_steering: String::new(),
            skills: Vec::new(),
            skill_selection: SkillSelection::Explicit,
            skill_delivery: SkillDelivery::NamesOnly,
            mcp_servers: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
            access_mode: BackendAccessMode::Unrestricted,
        }
    }

    /// Low-level Backend callers have no host stores. Host sessions must use
    /// resolve_spawn_config instead; this policy cannot start an agent actor.
    pub fn for_backend_session() -> Self {
        Self::empty(SpawnConfigPolicy::BackendOnly)
    }

    /// Naming, summaries, compaction inference, supervision and quota wakeups
    /// are isolated utility turns, not user agents: no steering, skills or MCP.
    pub(crate) fn inference_only() -> Self {
        let mut resolved = Self::empty(SpawnConfigPolicy::InferenceOnly);
        resolved.tool_policy = ToolPolicy::AllowList { tools: Vec::new() };
        resolved.access_mode = BackendAccessMode::ReadOnly;
        resolved
    }

    /// A failed agent record never reaches a backend, even if its source
    /// session or customization no longer exists.
    pub(crate) fn failed_startup() -> Self {
        Self::empty(SpawnConfigPolicy::FailedStartup)
    }

    /// Whether a plain `Resume` rebuilds this configuration faithfully.
    ///
    /// Only the user policy is reconstructed from the session record alone. A
    /// reviewer carries dedicated instructions, a read-only tool policy, its
    /// review MCP and a tool bridge that live entirely outside the record, so
    /// resuming one produces an ordinary user agent wearing its name. The
    /// remaining policies never describe a session a user has open.
    pub(crate) fn is_rebuilt_by_resume(&self) -> bool {
        matches!(self.policy, SpawnConfigPolicy::User)
    }

    pub(crate) fn assert_session_policy(&self, startup_failed: bool) {
        assert!(
            matches!(
                self.policy,
                SpawnConfigPolicy::User | SpawnConfigPolicy::Reviewer
            ) || (startup_failed && self.policy == SpawnConfigPolicy::FailedStartup),
            "agent session bypassed spawn resolution: {:?}",
            self.policy
        );
        tracing::debug!(policy = ?self.policy, startup_failed, "resolved session spawn policy");
    }
}

pub(crate) enum SpawnCustomization<'a> {
    /// Includes Default (when no agent is selected), workspace/store skills,
    /// user MCP servers, and host/project steering. Workflows are user agents.
    User(Option<&'a CustomAgentId>),
    /// The review role retains user steering, but only its dedicated prompt,
    /// read-only tools and review MCP: no Default role, skills or user MCP.
    Reviewer { instructions: String },
}

pub(crate) struct ResolveSpawnConfigRequest<'a> {
    pub backend_kind: BackendKind,
    pub project_id: Option<&'a ProjectId>,
    pub workspace_roots: &'a [String],
    pub customization: SpawnCustomization<'a>,
    pub built_in_mcp_servers: &'a [StartupMcpServer],
    pub custom_agent_store: &'a CustomAgentStore,
    pub mcp_server_store: &'a McpServerStore,
    pub steering_store: &'a SteeringStore,
    pub skill_store: &'a SkillStore,
}

pub(crate) fn resolve_spawn_config(
    request: ResolveSpawnConfigRequest<'_>,
) -> Result<ResolvedSpawnConfig, String> {
    let policy = match &request.customization {
        SpawnCustomization::User(_) => SpawnConfigPolicy::User,
        SpawnCustomization::Reviewer { .. } => SpawnConfigPolicy::Reviewer,
    };
    let mut resolved = ResolvedSpawnConfig::empty(policy);
    resolved.mcp_servers = request
        .built_in_mcp_servers
        .iter()
        .map(startup_mcp_server_to_protocol)
        .collect();
    resolved.steering_body = resolve_steering_body(request.steering_store, request.project_id)?;
    resolved.skill_delivery = crate::backend::skill_delivery(request.backend_kind);
    match &request.customization {
        SpawnCustomization::User(custom_agent_id) => {
            resolve_user_customization(&request, *custom_agent_id, &mut resolved)?;
        }
        SpawnCustomization::Reviewer { instructions } => {
            resolved.instructions = Some(instructions.clone());
            resolved.tool_policy = crate::review::reviewer::reviewer_tool_policy();
            resolved.access_mode = BackendAccessMode::ReadOnly;
        }
    }
    resolved.builtin_steering = crate::backend::builtin_steering_for_tools(
        &protocol_mcp_servers_to_startup(&resolved.mcp_servers),
    );
    Ok(resolved)
}

fn resolve_user_customization(
    request: &ResolveSpawnConfigRequest<'_>,
    custom_agent_id: Option<&CustomAgentId>,
    resolved: &mut ResolvedSpawnConfig,
) -> Result<(), String> {
    let mut mcp_servers = std::mem::take(&mut resolved.mcp_servers);
    let mut mcp_names = mcp_servers
        .iter()
        .map(|server| (server.name.clone(), server.id.clone()))
        .collect::<HashMap<_, _>>();

    let mut instructions = None;
    let mut skills = Vec::new();
    let mut skill_selection = SkillSelection::Explicit;
    let mut tool_policy = ToolPolicy::Unrestricted;

    let project_skills = crate::store::skills::scan_workspace_skills(request.workspace_roots)?;
    let mut project_skill_names = std::collections::HashSet::new();
    for skill in &project_skills {
        project_skill_names.insert(skill.name.clone());
    }

    // A spawn with no explicit custom agent uses the editable "Default"
    // builtin, so users can customize every plain chat from Settings →
    // Custom Agents. An explicit selection must exist; the implicit default
    // is best-effort (a deleted Default agent means no customization).
    let custom_agent = match custom_agent_id {
        Some(custom_agent_id) => Some(
            request
                .custom_agent_store
                .get(custom_agent_id)
                .ok_or_else(|| {
                    format!("cannot resolve missing custom agent {}", custom_agent_id)
                })?,
        ),
        None => request.custom_agent_store.get(&CustomAgentId(
            crate::store::custom_agents::DEFAULT_CUSTOM_AGENT_ID.to_owned(),
        )),
    };

    if let Some(custom_agent) = custom_agent {
        instructions = custom_agent.instructions.clone();
        tool_policy = custom_agent.tool_policy.clone();

        if is_default_custom_agent(&custom_agent) {
            skill_selection = SkillSelection::AllInstalled;
            skills.extend(project_skills);
            for skill in request.skill_store.list()? {
                if !project_skill_names.contains(&skill.name) {
                    skills.push(resolve_skill(request.skill_store, &skill.id)?);
                }
            }
            skills
                .sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
            for mcp_server in request.mcp_server_store.list()? {
                push_mcp_server(
                    &custom_agent.id,
                    mcp_server,
                    &mut mcp_names,
                    &mut mcp_servers,
                )?;
            }
        } else {
            skills.extend(project_skills);
            for skill_id in &custom_agent.skill_ids {
                let resolved = resolve_skill(request.skill_store, skill_id)?;
                if !project_skill_names.contains(&resolved.name) {
                    skills.push(resolved);
                }
            }
            skills
                .sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));

            for mcp_server_id in &custom_agent.mcp_server_ids {
                let mcp_server = request.mcp_server_store.get(mcp_server_id).ok_or_else(|| {
                    format!(
                        "custom agent {} references missing MCP server {}",
                        custom_agent.id, mcp_server_id
                    )
                })?;
                push_mcp_server(
                    &custom_agent.id,
                    mcp_server,
                    &mut mcp_names,
                    &mut mcp_servers,
                )?;
            }
        }
    } else if !project_skills.is_empty() {
        skills.extend(project_skills);
        skills.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
    }

    crate::backend::validate_tool_policy(request.backend_kind, &tool_policy)?;

    resolved.instructions = instructions;
    resolved.skills = skills;
    resolved.skill_selection = skill_selection;
    resolved.mcp_servers = mcp_servers;
    resolved.tool_policy = tool_policy;

    if !resolved.skills.is_empty() {
        // When a skill fails to show up in a session, this is the line that
        // says which id resolved to which directory, and whether the backend
        // was expected to discover it or to be handed its text.
        tracing::debug!(
            backend = ?request.backend_kind,
            selection = ?resolved.skill_selection,
            delivery = ?resolved.skill_delivery,
            skills = %resolved
                .skills
                .iter()
                .map(|skill| {
                    let summary = skill
                        .description
                        .as_deref()
                        .or(skill.title.as_deref())
                        .unwrap_or("no store metadata");
                    format!("{}@{} ({summary})", skill.id, skill.source_dir.display())
                })
                .collect::<Vec<_>>()
                .join(", "),
            "resolved session skills"
        );
    }

    Ok(())
}

pub(crate) fn protocol_mcp_servers_to_startup(
    mcp_servers: &[McpServerConfig],
) -> Vec<StartupMcpServer> {
    mcp_servers
        .iter()
        .map(|server| StartupMcpServer {
            name: server.name.clone(),
            supports_parallel_tool_calls: server.supports_parallel_tool_calls,
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

fn is_default_custom_agent(custom_agent: &CustomAgent) -> bool {
    custom_agent.id.0 == crate::store::custom_agents::DEFAULT_CUSTOM_AGENT_ID
}

fn push_mcp_server(
    custom_agent_id: &CustomAgentId,
    mcp_server: McpServerConfig,
    mcp_names: &mut HashMap<String, McpServerId>,
    mcp_servers: &mut Vec<McpServerConfig>,
) -> Result<(), String> {
    let name = mcp_server.name.clone();
    if RESERVED_MCP_SERVER_NAMES.contains(&name.as_str()) {
        return Err(format!(
            "custom agent {} references reserved MCP server name '{}'",
            custom_agent_id, name
        ));
    }
    if let Some(existing_id) = mcp_names.get(&name) {
        return Err(format!(
            "custom agent {} MCP server '{}' collides with existing server {}",
            custom_agent_id, name, existing_id
        ));
    }
    mcp_names.insert(name, mcp_server.id.clone());
    mcp_servers.push(mcp_server);
    Ok(())
}

fn startup_mcp_server_to_protocol(server: &StartupMcpServer) -> McpServerConfig {
    McpServerConfig {
        id: McpServerId(format!("builtin:{}", server.name)),
        name: server.name.clone(),
        supports_parallel_tool_calls: server.supports_parallel_tool_calls,
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
    let paths = skill_store.skill_paths(skill_id)?;
    Ok(ResolvedSkill::path_only(
        skill,
        paths.source_dir,
        paths.skill_md,
    ))
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
