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
    /// Inline body, non-empty only when the session's [`SkillDelivery`] loads
    /// bodies. Prefer [`ResolvedSkill::inline_body`], which cannot be read as
    /// "the body happens to be empty".
    pub body: String,
}

impl ResolvedSkill {
    /// The only place a `ResolvedSkill` is built from the store, so `body`
    /// cannot disagree with the delivery the session was resolved under.
    fn new(
        skill: protocol::Skill,
        paths: crate::store::skills::SkillPaths,
        delivery: SkillDelivery,
    ) -> Result<Self, String> {
        let mut resolved = Self {
            id: skill.id,
            name: skill.name,
            title: skill.title,
            description: skill.description,
            source_dir: paths.source_dir,
            skill_md_path: paths.skill_md,
            body: String::new(),
        };
        if delivery.loads_bodies() {
            resolved.body = resolved.load_body()?;
        }
        Ok(resolved)
    }

    /// The inline body, or `None` when this session's delivery never loaded
    /// one. The store rejects empty bodies, so an empty `body` always means
    /// "not delivered" rather than "delivered and blank".
    pub fn inline_body(&self) -> Option<&str> {
        let body = self.body.trim();
        (!body.is_empty()).then_some(body)
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

#[cfg(test)]
impl ResolvedSkill {
    /// A skill fixture with an inline body and no store behind it, for tests
    /// that exercise rendering rather than resolution.
    pub(crate) fn test_fixture(name: &str, body: &str) -> Self {
        let source_dir = PathBuf::from("/nonexistent/tyde-test-skills").join(name);
        Self {
            id: SkillId(name.to_string()),
            name: name.to_string(),
            title: None,
            description: None,
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
            BackendKind::Claude | BackendKind::Codex => Self::NativeDiscovery,
            // Hermes replaces the shared skill block with its own name-only
            // catalog, so every body it was handed was read and thrown away.
            BackendKind::Hermes => Self::NamesOnly,
            BackendKind::Tycode | BackendKind::Kiro | BackendKind::Antigravity => {
                Self::InlineBodies
            }
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
    ResolvedSkill::new(skill, paths, delivery)
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

        fn body_path(&self, name: &str) -> PathBuf {
            self.dir.join("skills").join(name).join("SKILL.md")
        }

        /// Delete a skill's `SKILL.md` while leaving it indexed.
        fn remove_skill_body(&self, name: &str) {
            let path = self.body_path(name);
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
    fn native_backends_carry_no_body_text_anywhere_in_the_resolved_config() {
        let fixture = StoreFixture::new("native-no-body");
        fixture.install_skill("lint", &format!("{BODY_SENTINEL}\n{}", "x".repeat(20_000)));
        fixture.install_skill("qa", BODY_SENTINEL);

        for backend_kind in [BackendKind::Claude, BackendKind::Codex] {
            let resolved = fixture.resolve(backend_kind, None).unwrap_or_else(|err| {
                panic!("{backend_kind:?} must resolve without a body: {err}")
            });

            assert_eq!(resolved.skill_delivery, SkillDelivery::NativeDiscovery);
            assert!(!resolved.skill_delivery.loads_bodies());
            assert_eq!(resolved.skills.len(), 2, "{backend_kind:?}");
            // The whole config, not just `body`: a sentinel anywhere in it
            // would mean some field is smuggling the text through.
            assert!(
                !format!("{resolved:?}").contains(BODY_SENTINEL),
                "{backend_kind:?} carried skill body text in its resolved config"
            );
            for skill in &resolved.skills {
                assert_eq!(skill.inline_body(), None);
                assert!(skill.source_dir.is_dir(), "{}", skill.source_dir.display());
                assert!(skill.skill_md_path.is_file());
                assert_eq!(skill.skill_md_path, skill.source_dir.join("SKILL.md"));
                assert_eq!(skill.id.0, skill.name);
                // Store metadata travels with the skill so an adapter can build
                // a catalog without reading anything.
                assert_eq!(
                    skill.description,
                    Some(format!("{} description", skill.name))
                );
            }
        }
    }

    /// The strongest available proof that native resolution never opens
    /// `SKILL.md`: make the file unreadable and resolve anyway. Deleting it
    /// would not prove this, because resolution legitimately stats the path.
    #[cfg(unix)]
    #[test]
    fn native_resolution_does_not_open_an_unreadable_body() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = StoreFixture::new("native-unreadable-body");
        fixture.install_skill("lint", BODY_SENTINEL);
        let body_path = fixture.body_path("lint");
        std::fs::set_permissions(&body_path, std::fs::Permissions::from_mode(0o000))
            .expect("make the body unreadable");

        if std::fs::read_to_string(&body_path).is_ok() {
            // Privileges that ignore file modes (running as root) cannot prove
            // this negative; the sentinel test above still covers the contract.
            return;
        }

        let resolved = fixture
            .resolve(BackendKind::Claude, None)
            .expect("native resolution must not open SKILL.md");
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].inline_body(), None);
        assert!(resolved.skills[0].skill_md_path.is_file());

        // The same store fails for a backend that must inline bodies, so the
        // fixture really is unreadable rather than quietly readable.
        let err = fixture
            .resolve(BackendKind::Kiro, None)
            .expect_err("inline delivery must fail on an unreadable body");
        assert!(err.contains("Failed to read skill body"), "{err}");
    }

    #[test]
    fn hermes_receives_names_without_reading_bodies() {
        let fixture = StoreFixture::new("hermes-names-only");
        fixture.install_skill("lint", BODY_SENTINEL);

        let resolved = fixture
            .resolve(BackendKind::Hermes, None)
            .expect("Hermes must resolve without reading bodies");

        assert_eq!(resolved.skill_delivery, SkillDelivery::NamesOnly);
        assert!(!resolved.skill_delivery.loads_bodies());
        assert!(!resolved.skill_delivery.renders_inline_bodies());
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].name, "lint");
        assert_eq!(resolved.skills[0].inline_body(), None);
        assert!(!format!("{resolved:?}").contains(BODY_SENTINEL));

        // Resolver through to the shared renderer: the name-only catalog Hermes
        // builds itself still has a name to build from, and nothing the shared
        // renderer emits contains a body.
        let rendered = crate::backend::render_combined_spawn_instructions(&resolved);
        if let Some(text) = rendered {
            assert!(!text.contains(BODY_SENTINEL), "{text}");
            assert!(!text.contains("Skill: "), "{text}");
        }
    }

    #[test]
    fn resolved_bodies_never_contradict_the_session_delivery() {
        let fixture = StoreFixture::new("delivery-invariant");
        fixture.install_skill("lint", BODY_SENTINEL);

        for backend_kind in [
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Hermes,
            BackendKind::Kiro,
            BackendKind::Tycode,
            BackendKind::Antigravity,
        ] {
            let resolved = fixture
                .resolve(backend_kind, None)
                .unwrap_or_else(|err| panic!("{backend_kind:?} resolution: {err}"));
            for skill in &resolved.skills {
                assert_eq!(
                    skill.inline_body().is_some(),
                    resolved.skill_delivery.loads_bodies(),
                    "{backend_kind:?} body presence disagrees with {:?}",
                    resolved.skill_delivery
                );
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
        assert!(resolved.skill_delivery.loads_bodies());
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].inline_body(), Some(BODY_SENTINEL));

        // A skill whose SKILL.md has gone missing fails the spawn rather than
        // starting a session with a silently empty skill — for every backend,
        // since a native one could not discover it either.
        fixture.remove_skill_body("lint");
        for backend_kind in [BackendKind::Kiro, BackendKind::Claude] {
            let err = fixture
                .resolve(backend_kind, None)
                .expect_err("resolution must fail when SKILL.md is gone");
            assert!(err.contains("cannot resolve skill lint"), "{err}");
        }
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
