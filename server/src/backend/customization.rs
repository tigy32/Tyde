use std::path::PathBuf;

use protocol::{BackendAccessMode, McpServerConfig, SkillId, ToolPolicy};

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
