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

/// One skill selected for a session: identity and location first, body only
/// when the backend has no way to read it for itself.
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
    payload: SkillPayload,
}

/// What a session was handed for one skill.
///
/// Private, and the only way to reach it is [`ResolvedSkill::inline_body`].
/// That is the whole point: no caller outside this module can put inline text
/// into a skill its backend is supposed to discover for itself, or add text to
/// one after the fact, because there is no public constructor that takes a body
/// and no field to assign.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillPayload {
    /// The backend opens `skill_md_path` itself when the model invokes the
    /// skill. No text travels with the session.
    Path,
    /// The backend has no discovery seam, so the text travels with the skill.
    Inline(String),
}

impl ResolvedSkill {
    /// A skill the backend finds for itself: identity and canonical locations,
    /// never any text. This is the only public constructor, so a
    /// `NativeDiscovery` or `NamesOnly` skill carrying inline text is not a
    /// state a caller can express.
    pub fn path_only(skill: protocol::Skill, source_dir: PathBuf, skill_md_path: PathBuf) -> Self {
        Self {
            id: skill.id,
            name: skill.name,
            title: skill.title,
            description: skill.description,
            source_dir,
            skill_md_path,
            payload: SkillPayload::Path,
        }
    }

    /// The only place a skill is built from the store, so its payload cannot
    /// disagree with the delivery the session was resolved under.
    fn resolved(
        skill: protocol::Skill,
        paths: crate::store::skills::SkillPaths,
        delivery: SkillDelivery,
    ) -> Result<Self, String> {
        let mut resolved = Self::path_only(skill, paths.source_dir, paths.skill_md);
        if delivery.loads_bodies() {
            resolved.payload = SkillPayload::Inline(resolved.load_body()?);
        }
        Ok(resolved)
    }

    /// The inline body, or `None` when this session's delivery never loaded
    /// one. Absence is a distinct state rather than an empty string, so
    /// "discovered on demand" cannot be misread as "has no instructions".
    pub fn inline_body(&self) -> Option<&str> {
        match &self.payload {
            SkillPayload::Path => None,
            SkillPayload::Inline(body) => Some(body.as_str()),
        }
    }

    /// Read this skill's `SKILL.md` on demand.
    ///
    /// Native-discovery sessions never call this: the backend opens the file
    /// itself when the model invokes the skill, so the body costs nothing until
    /// then. It exists for backends with no discovery seam, and for Claude over
    /// SSH, where a locally materialized directory is invisible to the remote
    /// CLI. Keeping it an explicit call means every prompt that still pays for
    /// an inline body is greppable from one place.
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
/// This is the transport fact. The two policies that follow from it —
/// whether resolution reads bodies, and whether the shared renderer inlines
/// them — are separate predicates, because they are not the same question: a
/// backend can want no bodies in its prompt for reasons that have nothing to do
/// with whether it can find the skill on disk.
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
    /// The backend has no discovery seam, so bodies are rendered into its spawn
    /// instructions and are loaded during resolution — a read failure surfaces
    /// at spawn rather than halfway through building a prompt.
    InlineBodies,
}

impl SkillDelivery {
    pub(crate) fn for_backend(backend_kind: BackendKind) -> Self {
        match backend_kind {
            // Tycode discovers `<workspace root>/.tycode/skills/<name>` itself,
            // lists name and description in its system prompt, and loads a body
            // only when the model calls `invoke_skill`. Inlining bodies here as
            // well put every selected skill's full text in the prompt *and* in
            // the catalog, which is the duplication native discovery exists to
            // avoid.
            BackendKind::Claude | BackendKind::Codex | BackendKind::Tycode => Self::NativeDiscovery,
            // Hermes replaces the shared skill block with its own name-only
            // catalog, so every body it was handed was read and thrown away.
            BackendKind::Hermes => Self::NamesOnly,
            BackendKind::Acp | BackendKind::Antigravity => Self::InlineBodies,
        }
    }

    /// Whether resolution must read each selected skill's `SKILL.md`.
    pub fn loads_bodies(self) -> bool {
        match self {
            Self::NativeDiscovery | Self::NamesOnly => false,
            Self::InlineBodies => true,
        }
    }

    /// Whether the shared spawn-instruction renderer inlines skill bodies.
    pub fn renders_inline_bodies(self) -> bool {
        match self {
            Self::NativeDiscovery | Self::NamesOnly => false,
            Self::InlineBodies => true,
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
            skill_delivery: SkillDelivery::InlineBodies,
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
                skills.push(resolve_skill(
                    request.skill_store,
                    &skill.id,
                    skill_delivery,
                )?);
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
                skills.push(resolve_skill(
                    request.skill_store,
                    skill_id,
                    skill_delivery,
                )?);
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

fn resolve_skill(
    skill_store: &SkillStore,
    skill_id: &SkillId,
    delivery: SkillDelivery,
) -> Result<ResolvedSkill, String> {
    let skill = skill_store
        .get(skill_id)
        .ok_or_else(|| format!("cannot resolve missing skill {}", skill_id))?;
    let paths = skill_store.skill_paths(skill_id)?;
    ResolvedSkill::resolved(skill, paths, delivery)
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
