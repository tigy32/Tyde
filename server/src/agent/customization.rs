use std::collections::HashMap;
use std::path::PathBuf;

use protocol::{
    BackendAccessMode, BackendKind, CustomAgent, CustomAgentId, McpServerConfig, McpServerId,
    McpTransportConfig, ProjectId, SkillId, ToolPolicy,
};

use crate::backend::{StartupMcpServer, StartupMcpTransport};
use crate::store::custom_agents::CustomAgentStore;
use crate::store::mcp_servers::{McpServerStore, RESERVED_MCP_SERVER_NAMES};
use crate::store::skills::SkillStore;
use crate::store::steering::SteeringStore;

/// One skill selected for a session: identity and canonical on-disk location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub id: SkillId,
    /// Directory name in the Tyde store, which is also the name the skill is
    /// addressed by once a backend discovers it natively. It is whatever the
    /// user's store contains: an adapter whose backend cannot address a given
    /// name must normalize it or report it, not assume it is already safe.
    pub name: String,
    /// Display title from the store's metadata, when it has one.
    pub title: Option<String>,
    /// One-line summary from the store's metadata, when it has one.
    ///
    /// `None` is normal and is not a defect to work around: Claude 2.1.220
    /// loads a `SKILL.md` with no frontmatter at all, so nothing here requires
    /// a description to exist. Adapters that want richer catalog text should
    /// use it when present and fall back to the name.
    pub description: Option<String>,
    /// Canonical skill directory, proven to sit inside the Tyde skill store.
    pub source_dir: PathBuf,
    /// Canonical `<source_dir>/SKILL.md`, proven to be a regular file inside
    /// `source_dir`.
    pub skill_md_path: PathBuf,
}

impl ResolvedSkill {
    /// A skill the backend finds for itself: identity and canonical locations,
    /// never any inline text.
    pub fn path_only(skill: protocol::Skill, source_dir: PathBuf, skill_md_path: PathBuf) -> Self {
        Self {
            id: skill.id,
            name: skill.name,
            title: skill.title,
            description: skill.description,
            source_dir,
            skill_md_path,
        }
    }

    /// Read this skill's `SKILL.md` on demand.
    ///
    /// Native-discovery adapters use this only while creating the private
    /// on-disk projection their backend discovers. The text is never rendered
    /// into spawn instructions or a user message.
    pub fn load_body(&self) -> Result<String, String> {
        std::fs::read_to_string(&self.skill_md_path).map_err(|err| {
            format!(
                "Failed to read skill body {}: {err}",
                self.skill_md_path.display()
            )
        })
    }
}

/// Why a session holds the skills it holds.
///
/// Adapters need this to scope what they tell the model: advertising "these
/// skills were chosen for you" is only true for an explicit selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSelection {
    /// The builtin Default agent: every installed skill.
    AllInstalled,
    /// A custom agent naming its skills, or no customization at all.
    Explicit,
}

/// How a backend receives the skills resolved for a session.
///
/// This is the transport fact. Full skill bodies are never a delivery mode:
/// backends either discover selected skills on demand or receive names only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDelivery {
    /// The adapter exposes `source_dir` through the backend's own on-demand
    /// skill discovery.
    NativeDiscovery,
    /// The adapter names the selected skills and the backend finds their text
    /// through its own mechanism. Tyde hands over no path and no body — which
    /// is a real gap when the backend's mechanism looks somewhere other than
    /// Tyde's store, and the adapter owns closing it.
    NamesOnly,
}

impl SkillDelivery {
    pub(crate) fn for_backend(backend_kind: BackendKind) -> Self {
        match backend_kind {
            BackendKind::Tycode => Self::NamesOnly,
            // Tycode discovers `<workspace root>/.tycode/skills/<name>` itself,
            // lists name and description in its system prompt, and loads a body
            // only when the model calls `invoke_skill`. Inlining bodies here as
            // well put every selected skill's full text in the prompt *and* in
            // the catalog, which is the duplication native discovery exists to
            // avoid.
            BackendKind::Claude | BackendKind::Codex | BackendKind::Antigravity => {
                Self::NativeDiscovery
            }
            // Hermes provides its own name-only catalog. Kiro has no native
            // selected-skill projection yet, so its resolved names remain
            // available to a future adapter without loading any body today.
            BackendKind::Hermes | BackendKind::Kiro | BackendKind::Grok => Self::NamesOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpawnConfig {
    pub instructions: Option<String>,
    pub steering_body: String,
    pub skills: Vec<ResolvedSkill>,
    pub skill_selection: SkillSelection,
    pub skill_delivery: SkillDelivery,
    pub mcp_servers: Vec<McpServerConfig>,
    pub tool_policy: ToolPolicy,
    pub access_mode: BackendAccessMode,
}

impl Default for ResolvedSpawnConfig {
    fn default() -> Self {
        Self {
            instructions: None,
            steering_body: String::new(),
            skills: Vec::new(),
            // Explicit is the safe default: an adapter must never advertise
            // skills it was not handed.
            skill_selection: SkillSelection::Explicit,
            skill_delivery: SkillDelivery::NamesOnly,
            mcp_servers: Vec::new(),
            tool_policy: ToolPolicy::Unrestricted,
            access_mode: BackendAccessMode::Unrestricted,
        }
    }
}

pub(crate) struct ResolveSpawnConfigRequest<'a> {
    pub backend_kind: BackendKind,
    pub project_id: Option<&'a ProjectId>,
    pub custom_agent_id: Option<&'a CustomAgentId>,
    pub built_in_mcp_servers: &'a [StartupMcpServer],
    pub custom_agent_store: &'a CustomAgentStore,
    pub mcp_server_store: &'a McpServerStore,
    pub steering_store: &'a SteeringStore,
    pub skill_store: &'a SkillStore,
}

pub(crate) fn resolve_spawn_config(
    request: ResolveSpawnConfigRequest<'_>,
) -> Result<ResolvedSpawnConfig, String> {
    let mut mcp_servers = request
        .built_in_mcp_servers
        .iter()
        .map(startup_mcp_server_to_protocol)
        .collect::<Vec<_>>();
    let mut mcp_names = mcp_servers
        .iter()
        .map(|server| (server.name.clone(), server.id.clone()))
        .collect::<HashMap<_, _>>();

    let mut instructions = None;
    let mut skills = Vec::new();
    let mut skill_selection = SkillSelection::Explicit;
    let skill_delivery = SkillDelivery::for_backend(request.backend_kind);
    let mut tool_policy = ToolPolicy::Unrestricted;

    // A spawn with no explicit custom agent uses the editable "Default"
    // builtin, so users can customize every plain chat from Settings →
    // Custom Agents. An explicit selection must exist; the implicit default
    // is best-effort (a deleted Default agent means no customization).
    let custom_agent = match request.custom_agent_id {
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
            for skill in request.skill_store.list()? {
                skills.push(resolve_skill(request.skill_store, &skill.id)?);
            }
            for mcp_server in request.mcp_server_store.list()? {
                push_mcp_server(
                    &custom_agent.id,
                    mcp_server,
                    &mut mcp_names,
                    &mut mcp_servers,
                )?;
            }
        } else {
            for skill_id in &custom_agent.skill_ids {
                skills.push(resolve_skill(request.skill_store, skill_id)?);
            }

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
    }

    match &tool_policy {
        ToolPolicy::Unrestricted => {}
        ToolPolicy::AllowList { .. } | ToolPolicy::DenyList { .. } => {
            if request.backend_kind != BackendKind::Claude {
                return Err(format!(
                    "backend {:?} does not support tool policy {:?}",
                    request.backend_kind, tool_policy
                ));
            }
        }
    }

    let steering_body = resolve_steering_body(request.steering_store, request.project_id)?;

    let resolved = ResolvedSpawnConfig {
        instructions,
        steering_body,
        skills,
        skill_selection,
        skill_delivery,
        mcp_servers,
        tool_policy,
        access_mode: BackendAccessMode::Unrestricted,
    };

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

    Ok(resolved)
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
