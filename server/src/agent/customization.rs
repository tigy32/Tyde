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
    /// addressed by once a backend discovers it natively.
    pub name: String,
    /// Canonical skill directory, proven to sit inside the Tyde skill store.
    pub source_dir: PathBuf,
    /// Canonical `<source_dir>/SKILL.md`.
    pub skill_md_path: PathBuf,
    /// Inline body. Empty under [`SkillDelivery::NativeDiscovery`] — the
    /// backend reads `skill_md_path` itself when the model invokes the skill.
    /// Use [`ResolvedSkill::load_body`] rather than assuming this is populated.
    pub body: String,
}

impl ResolvedSkill {
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

#[cfg(test)]
impl ResolvedSkill {
    /// A skill fixture with an inline body and no store behind it, for tests
    /// that exercise rendering rather than resolution.
    pub(crate) fn test_fixture(name: &str, body: &str) -> Self {
        let source_dir = PathBuf::from("/nonexistent/tyde-test-skills").join(name);
        Self {
            id: SkillId(name.to_string()),
            name: name.to_string(),
            skill_md_path: source_dir.join("SKILL.md"),
            source_dir,
            body: body.to_string(),
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillDelivery {
    /// The adapter exposes `source_dir` through the backend's own on-demand
    /// skill discovery. Resolution never reads a `SKILL.md`.
    NativeDiscovery,
    /// The backend has no discovery seam, so bodies are rendered into its spawn
    /// instructions and are loaded during resolution — a read failure surfaces
    /// at spawn rather than halfway through building a prompt.
    InlineBodies,
}

impl SkillDelivery {
    pub(crate) fn for_backend(backend_kind: BackendKind) -> Self {
        match backend_kind {
            BackendKind::Claude | BackendKind::Codex => Self::NativeDiscovery,
            BackendKind::Tycode
            | BackendKind::Kiro
            | BackendKind::Antigravity
            | BackendKind::Hermes => Self::InlineBodies,
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
                skills.push(resolve_skill(request.skill_store, &skill.id, skill_delivery)?);
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
                skills.push(resolve_skill(request.skill_store, skill_id, skill_delivery)?);
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
                .map(|skill| format!("{}@{}", skill.id, skill.source_dir.display()))
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
    let mut resolved = ResolvedSkill {
        id: skill.id,
        name: skill.name,
        source_dir: paths.source_dir,
        skill_md_path: paths.skill_md,
        body: String::new(),
    };
    if delivery == SkillDelivery::InlineBodies {
        resolved.body = resolved.load_body()?;
    }
    Ok(resolved)
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Skill;

    const BODY_SENTINEL: &str = "SKILL_BODY_SENTINEL_MUST_NOT_BE_EAGERLY_LOADED";

    struct StoreFixture {
        dir: PathBuf,
        custom_agents: CustomAgentStore,
        mcp_servers: McpServerStore,
        steering: SteeringStore,
        skills: SkillStore,
    }

    impl StoreFixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("tyde-customization-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir)
                .unwrap_or_else(|err| panic!("create fixture dir {}: {err}", dir.display()));
            let custom_agents = CustomAgentStore::load(dir.join("custom_agents.json"))
                .expect("load custom agent store");
            let mcp_servers =
                McpServerStore::load(dir.join("mcp_servers.json")).expect("load mcp server store");
            let steering =
                SteeringStore::load(dir.join("steering.json")).expect("load steering store");
            let skills = SkillStore::load(dir.join("skills.json"), dir.join("skills"))
                .expect("load skill store");
            Self {
                dir,
                custom_agents,
                mcp_servers,
                steering,
                skills,
            }
        }

        fn install_skill(&self, name: &str, body: &str) -> SkillId {
            let id = SkillId(name.to_string());
            self.skills
                .upsert(
                    Skill {
                        id: id.clone(),
                        name: name.to_string(),
                        title: None,
                        description: Some(format!("{name} description")),
                    },
                    body.to_string(),
                )
                .unwrap_or_else(|err| panic!("install skill {name}: {err}"));
            id
        }

        /// Delete a skill's `SKILL.md` while leaving it indexed, so any attempt
        /// to read a body fails loudly instead of succeeding by accident.
        fn remove_skill_body(&self, name: &str) {
            let path = self.dir.join("skills").join(name).join("SKILL.md");
            std::fs::remove_file(&path)
                .unwrap_or_else(|err| panic!("remove {}: {err}", path.display()));
        }

        fn resolve(
            &self,
            backend_kind: BackendKind,
            custom_agent_id: Option<&CustomAgentId>,
        ) -> Result<ResolvedSpawnConfig, String> {
            resolve_spawn_config(ResolveSpawnConfigRequest {
                backend_kind,
                project_id: None,
                custom_agent_id,
                built_in_mcp_servers: &[],
                custom_agent_store: &self.custom_agents,
                mcp_server_store: &self.mcp_servers,
                steering_store: &self.steering,
                skill_store: &self.skills,
            })
        }
    }

    impl Drop for StoreFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn native_backends_resolve_skills_without_reading_any_body() {
        let fixture = StoreFixture::new("native-no-body");
        fixture.install_skill("lint", &format!("{BODY_SENTINEL}\n{}", "x".repeat(20_000)));
        fixture.install_skill("qa", BODY_SENTINEL);
        // Nothing may read these files during resolution, so remove them: a
        // resolution that still succeeds cannot have read a body.
        fixture.remove_skill_body("lint");
        fixture.remove_skill_body("qa");

        for backend_kind in [BackendKind::Claude, BackendKind::Codex] {
            let resolved = fixture.resolve(backend_kind, None).unwrap_or_else(|err| {
                panic!("{backend_kind:?} must resolve without a body: {err}")
            });

            assert_eq!(resolved.skill_delivery, SkillDelivery::NativeDiscovery);
            assert_eq!(resolved.skills.len(), 2, "{backend_kind:?}");
            for skill in &resolved.skills {
                assert!(
                    skill.body.is_empty(),
                    "{backend_kind:?} carried an inline body for {}",
                    skill.name
                );
                assert!(skill.source_dir.is_dir(), "{}", skill.source_dir.display());
                assert_eq!(skill.skill_md_path, skill.source_dir.join("SKILL.md"));
                assert_eq!(skill.id.0, skill.name);
            }
        }
    }

    #[test]
    fn resolved_skill_directories_stay_inside_the_store() {
        let fixture = StoreFixture::new("containment");
        fixture.install_skill("lint", BODY_SENTINEL);

        let resolved = fixture
            .resolve(BackendKind::Claude, None)
            .expect("resolve for Claude");
        let skill = resolved.skills.first().expect("one resolved skill");

        let canonical_root =
            std::fs::canonicalize(fixture.dir.join("skills")).expect("canonicalize store root");
        assert!(
            skill.source_dir.starts_with(&canonical_root),
            "{} escaped {}",
            skill.source_dir.display(),
            canonical_root.display()
        );
        // The body is still reachable, but only through an explicit call.
        assert_eq!(skill.load_body().expect("lazy body"), BODY_SENTINEL);
    }

    #[test]
    fn legacy_backends_still_receive_inline_bodies() {
        let fixture = StoreFixture::new("legacy-inline");
        fixture.install_skill("lint", BODY_SENTINEL);

        let resolved = fixture
            .resolve(BackendKind::Kiro, None)
            .expect("resolve for Kiro");
        assert_eq!(resolved.skill_delivery, SkillDelivery::InlineBodies);
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].body, BODY_SENTINEL);

        // And the read is real: without the file, a legacy spawn fails rather
        // than silently starting with an empty skill.
        fixture.remove_skill_body("lint");
        let err = fixture
            .resolve(BackendKind::Kiro, None)
            .expect_err("legacy resolution must fail when a body is unreadable");
        assert!(err.contains("Failed to read skill body"), "{err}");
    }

    #[test]
    fn explicit_custom_agent_resolves_only_its_selected_skills() {
        let fixture = StoreFixture::new("explicit-selection");
        let lint = fixture.install_skill("lint", BODY_SENTINEL);
        fixture.install_skill("qa", BODY_SENTINEL);

        let custom_agent_id = CustomAgentId("reviewer".to_string());
        fixture
            .custom_agents
            .upsert(CustomAgent {
                id: custom_agent_id.clone(),
                name: "Reviewer".to_string(),
                description: "Reviews changes".to_string(),
                instructions: None,
                skill_ids: vec![lint],
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            })
            .expect("upsert custom agent");

        let explicit = fixture
            .resolve(BackendKind::Claude, Some(&custom_agent_id))
            .expect("resolve custom agent");
        assert_eq!(explicit.skill_selection, SkillSelection::Explicit);
        assert_eq!(
            explicit
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["lint"]
        );

        let default = fixture
            .resolve(BackendKind::Claude, None)
            .expect("resolve default agent");
        assert_eq!(default.skill_selection, SkillSelection::AllInstalled);
        assert_eq!(
            default
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["lint", "qa"]
        );
    }
}
