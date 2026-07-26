//! Claude native skill discovery: one session-owned inline plugin.
//!
//! Claude Code discovers skills from a plugin directory passed with
//! `--plugin-dir-no-mcp`/`--plugin-dir`. Tyde materializes a session-scoped
//! plugin root whose `skills/` folder is a farm of **directory symlinks** into
//! the Tyde skill store, so no body is copied and the store is never written.
//!
//! Verified against Claude Code 2.1.220 in the exact
//! `--print --output-format stream-json` mode Tyde spawns (evidence:
//! `claude-live-smoke.md`):
//!
//! - Skills reach the model as `tyde-skills:<name>` and are listed in the
//!   `system/init` frame, which the CLI emits before any provider request.
//! - Bodies load lazily, at `Skill` invocation, not at boot.
//! - Relative links inside a body resolve through the directory symlink.
//! - `plugin.json` must **not** declare `skills`; declaring it shadows the
//!   auto-loaded folder and turns manifest-relative paths into a traversal
//!   check.
//!
//! Loading a plugin makes the Claude CLI record bounded usage telemetry in
//! `~/.claude.json` (`pluginUsage`, `skillUsage`). That is Claude CLI
//! behaviour, not a Tyde write: Tyde never edits the user's settings or skill
//! store. The keys are bounded — the plugin name is fixed, and `skillUsage`
//! gains at most one key per distinct skill ever invoked.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::agent::customization::{ResolvedSkill, SkillSelection};

/// Fixed plugin name. Skills are addressed as `tyde-skills:<name>`.
pub(crate) const TYDE_SKILLS_PLUGIN_NAME: &str = "tyde-skills";

/// Frontmatter keys that would let a skill contribute an executable or
/// long-lived component rather than instructions. A Tyde skill farm must stay
/// inert, so a skill declaring any of these is refused visibly rather than
/// symlinked. The live smoke confirmed a minimal-manifest farm contributes
/// zero hooks, agents, commands, MCP servers, LSP servers, and output styles;
/// this keeps it that way when the *skill* is the thing declaring them.
const REFUSED_FRONTMATTER_KEYS: &[&str] = &[
    "hooks",
    "mcp",
    "mcpservers",
    "mcp_servers",
    "lsp",
    "lspservers",
    "lsp_servers",
    "monitors",
    "agents",
    "commands",
    "workflows",
    "outputstyles",
    "output_styles",
    "statusline",
];

/// Which CLI flag carries the plugin root.
///
/// `--plugin-dir-no-mcp` is preferred: it tells the CLI not to read the
/// plugin's `.mcp.json`, so a farm can never smuggle a server in even if a
/// future store layout grows one. The live probe of 2.1.220 found the flag
/// **works but is hidden from `--help`**, so its presence cannot be proven by
/// probing help text — only `--plugin-dir` can. Tyde therefore prefers the
/// hidden flag and falls back once, per process, if a CLI rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudePluginFlag {
    NoMcp,
    PluginDir,
}

impl ClaudePluginFlag {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoMcp => "--plugin-dir-no-mcp",
            Self::PluginDir => "--plugin-dir",
        }
    }
}

/// Does this CLI's `--help` advertise plugin sideloading at all?
///
/// `--plugin-dir` is documented, so its absence is a real signal that the CLI
/// cannot load a session plugin. Absence must be reported, never worked around
/// by inlining bodies again.
pub(crate) fn help_text_supports_plugin_dir(help: &str) -> bool {
    help.contains("--plugin-dir")
}

/// Error text for a CLI that cannot sideload a plugin. Names the flag rather
/// than a version: one installed CLI cannot establish a floor version, and a
/// wrong version number in an error message is worse than none.
pub(crate) fn unsupported_plugin_dir_error() -> String {
    format!(
        "This Claude CLI does not support `--plugin-dir`, so Tyde cannot expose \
         installed skills to it. Skills are discovered natively from a \
         session-scoped `{TYDE_SKILLS_PLUGIN_NAME}` plugin; Tyde will not fall \
         back to pasting skill bodies into the prompt. Upgrade the Claude CLI, \
         or remove Tyde skills from this agent to start without them."
    )
}

/// Does this stderr line say the CLI rejected the hidden no-MCP flag?
///
/// Commander reports an unknown option and exits before the session starts, so
/// the signature is stable and cheap to match. Deliberately narrow: it must
/// name the flag, so an unrelated startup failure never silently downgrades the
/// flag Tyde uses.
pub(crate) fn stderr_rejects_no_mcp_flag(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    lowered.contains("plugin-dir-no-mcp")
        && (lowered.contains("unknown option")
            || lowered.contains("unknown argument")
            || lowered.contains("unrecognized option")
            || lowered.contains("unknown or unexpected option"))
}

/// A materialized session plugin root. Dropping it unlinks the root.
#[derive(Debug)]
pub(crate) struct ClaudeSkillPlugin {
    root: PathBuf,
    /// `tyde-skills:<name>` for every skill actually linked, sorted.
    exposed: Vec<String>,
}

impl ClaudeSkillPlugin {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn exposed(&self) -> &[String] {
        &self.exposed
    }

    /// Build `<parent>/<dir_name>` containing a minimal manifest and one
    /// directory symlink per addressable skill, plus the reason for every skill
    /// that was refused.
    ///
    /// The plugin is `None` when nothing is left to expose, so a session with
    /// no usable skills spawns without a plugin flag instead of pointing the
    /// CLI at an empty root. Refusals travel separately from the plugin
    /// precisely so that "every skill was refused" cannot be mistaken for "this
    /// session had no skills".
    pub(crate) fn materialize_with_warnings(
        parent: &Path,
        dir_name: &str,
        skills: &[ResolvedSkill],
    ) -> Result<(Option<Self>, Vec<String>), String> {
        let mut warnings = Vec::new();
        let mut linkable = Vec::new();
        let mut claimed: BTreeSet<&str> = BTreeSet::new();

        for skill in skills {
            if let Err(reason) = skill_name_is_addressable(&skill.name) {
                warnings.push(format!("skill '{}' is not exposed: {reason}", skill.name));
                continue;
            }
            match refused_frontmatter_keys(&skill.skill_md_path) {
                Ok(keys) if !keys.is_empty() => {
                    warnings.push(format!(
                        "skill '{}' is not exposed: its SKILL.md frontmatter declares {} , \
                         which would activate an executable plugin component",
                        skill.name,
                        keys.join(", ")
                    ));
                    continue;
                }
                Ok(_) => {}
                Err(err) => {
                    warnings.push(format!(
                        "skill '{}' is not exposed: could not read its SKILL.md frontmatter: {err}",
                        skill.name
                    ));
                    continue;
                }
            }
            if !claimed.insert(skill.name.as_str()) {
                warnings.push(format!(
                    "skill '{}' is not exposed: another selected skill already claims that name \
                     in the '{TYDE_SKILLS_PLUGIN_NAME}' namespace",
                    skill.name
                ));
                continue;
            }
            linkable.push(skill);
        }

        if linkable.is_empty() {
            return Ok((None, warnings));
        }

        let root = parent.join(dir_name);
        if root.exists() {
            return Err(format!(
                "Claude skill plugin root {} already exists",
                root.display()
            ));
        }
        let manifest_dir = root.join(".claude-plugin");
        let skills_dir = root.join("skills");
        std::fs::create_dir_all(&manifest_dir)
            .map_err(|err| format!("Failed to create {}: {err}", manifest_dir.display()))?;
        std::fs::create_dir_all(&skills_dir)
            .map_err(|err| format!("Failed to create {}: {err}", skills_dir.display()))?;
        std::fs::write(manifest_dir.join("plugin.json"), plugin_manifest_json())
            .map_err(|err| format!("Failed to write Claude plugin manifest: {err}"))?;

        let mut exposed = Vec::new();
        for skill in linkable {
            let link = skills_dir.join(&skill.name);
            symlink_dir(&skill.source_dir, &link).map_err(|err| {
                format!(
                    "Failed to link skill '{}' into the Claude plugin root: {err}",
                    skill.name
                )
            })?;
            exposed.push(namespaced_skill_name(&skill.name));
        }
        exposed.sort();

        Ok((Some(Self { root, exposed }), warnings))
    }

    /// Unlink the session root.
    ///
    /// Every entry of `skills/` is a symlink into the user's real skill store.
    /// This removes **links only**: each entry is checked with
    /// `symlink_metadata` (which does not follow) and unlinked with
    /// `remove_file`. A recursive delete is never issued against a link, so a
    /// bug here cannot reach a target. Anything unexpected in `skills/` is left
    /// in place and reported rather than deleted.
    pub(crate) fn cleanup(&self) -> Result<(), String> {
        // Idempotent: shutdown cleans up explicitly so the root does not sit in
        // TMPDIR until the last handle drops, and `Drop` then runs again.
        if !self.root.exists() {
            return Ok(());
        }
        let skills_dir = self.root.join("skills");
        let mut left_behind = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_symlink = std::fs::symlink_metadata(&path)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_symlink {
                    left_behind.push(path);
                    continue;
                }
                if let Err(err) = std::fs::remove_file(&path) {
                    left_behind.push(path.clone());
                    tracing::warn!("Failed to unlink Claude skill link {}: {err}", path.display());
                }
            }
        }
        if !left_behind.is_empty() {
            return Err(format!(
                "Claude skill plugin root {} kept {} entry/entries that are not symlinks; \
                 left in place rather than deleted",
                self.root.display(),
                left_behind.len()
            ));
        }
        let _ = std::fs::remove_dir(&skills_dir);
        let _ = std::fs::remove_file(self.root.join(".claude-plugin").join("plugin.json"));
        let _ = std::fs::remove_dir(self.root.join(".claude-plugin"));
        std::fs::remove_dir(&self.root)
            .map_err(|err| format!("Failed to remove {}: {err}", self.root.display()))
    }
}

impl Drop for ClaudeSkillPlugin {
    fn drop(&mut self) {
        if let Err(err) = self.cleanup() {
            tracing::warn!("Claude skill plugin cleanup incomplete: {err}");
        }
    }
}

/// Minimal manifest. `skills` is deliberately absent: declaring it shadows the
/// auto-loaded `skills/` folder and turns manifest-relative paths into a
/// traversal check, which breaks the symlink farm.
fn plugin_manifest_json() -> String {
    format!(
        "{{\n  \"name\": \"{TYDE_SKILLS_PLUGIN_NAME}\",\n  \
         \"description\": \"Skills installed in Tyde\",\n  \
         \"version\": \"0.0.0\"\n}}\n"
    )
}

pub(crate) fn namespaced_skill_name(name: &str) -> String {
    format!("{TYDE_SKILLS_PLUGIN_NAME}:{name}")
}

/// Can Claude address this skill name inside the `tyde-skills` namespace?
///
/// The shared store deliberately does not pre-filter names — a leading `.` or a
/// `:` stays valid there, because rejecting one would silently drop a skill a
/// user already has. Deciding addressability is this adapter's job, per
/// session, and a refusal is reported rather than hidden.
pub(crate) fn skill_name_is_addressable(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the name is empty".to_string());
    }
    if name.trim() != name {
        return Err("the name has leading or trailing whitespace".to_string());
    }
    if name.contains(':') {
        return Err(
            "the name contains ':', which Claude reserves for plugin namespacing".to_string(),
        );
    }
    if name.contains('/') || name.contains('\\') {
        return Err("the name contains a path separator".to_string());
    }
    if name == "." || name == ".." {
        return Err("the name is a relative path segment".to_string());
    }
    if name.starts_with('.') {
        return Err("the name starts with '.', which Claude's skill loader skips".to_string());
    }
    if name.chars().any(char::is_whitespace) {
        return Err("the name contains whitespace".to_string());
    }
    Ok(())
}

/// Top-level frontmatter keys that would activate an executable component.
///
/// Frontmatter is optional — 2.1.220 loads a `SKILL.md` without any — so a file
/// with no frontmatter yields no keys and is accepted.
pub(crate) fn refused_frontmatter_keys(skill_md: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(skill_md)
        .map_err(|err| format!("{}: {err}", skill_md.display()))?;
    Ok(refused_keys_in_frontmatter(&text))
}

fn refused_keys_in_frontmatter(text: &str) -> Vec<String> {
    let Some(frontmatter) = extract_frontmatter(text) else {
        return Vec::new();
    };
    let Ok(serde_yaml::Value::Mapping(mapping)) = serde_yaml::from_str(frontmatter) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for key in mapping.keys() {
        let Some(key) = key.as_str() else { continue };
        let normalized = key.trim().to_ascii_lowercase();
        if REFUSED_FRONTMATTER_KEYS.contains(&normalized.as_str()) {
            found.push(key.trim().to_string());
        }
    }
    found.sort();
    found
}

fn extract_frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n").or_else(|| {
        text.strip_prefix("---\r\n")
            .or_else(|| text.strip_prefix("\u{feff}---\n"))
    })?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))
        .or_else(|| rest.strip_suffix("\n---").map(str::len))?;
    Some(&rest[..end])
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// The steering overlay describing how to reach Tyde skills.
///
/// `AllInstalled` gets one constant-size paragraph and **no enumeration**:
/// Claude's own skill listing already carries every name and description, so
/// re-listing them would rebuild the duplication this work removes. `Explicit`
/// enumerates the selected names, because a custom agent's selection is a
/// deliberate statement of intent that the model should see. Neither carries a
/// body.
pub(crate) fn native_skill_overlay(selection: SkillSelection, skills: &[ResolvedSkill]) -> String {
    match selection {
        SkillSelection::AllInstalled => format!(
            "Skills installed in Tyde are available through the \
             `{TYDE_SKILLS_PLUGIN_NAME}` plugin and are addressed as \
             `{TYDE_SKILLS_PLUGIN_NAME}:<name>`. Their names and descriptions \
             are already listed among your available skills; read a skill only \
             when you invoke it."
        ),
        SkillSelection::Explicit => {
            let mut lines = vec![format!(
                "This agent selected these Tyde skills, available through the \
                 `{TYDE_SKILLS_PLUGIN_NAME}` plugin as `{TYDE_SKILLS_PLUGIN_NAME}:<name>`:"
            )];
            for skill in skills {
                match skill.description.as_deref().map(str::trim) {
                    Some(description) if !description.is_empty() => {
                        lines.push(format!("- {} — {description}", skill.name));
                    }
                    _ => lines.push(format!("- {}", skill.name)),
                }
            }
            lines.join("\n")
        }
    }
}

/// Names Tyde expected to see that the CLI did not report in its `init` frame.
///
/// The `init` frame is emitted locally before any provider request, so this
/// check costs nothing and catches a skill the CLI dropped — notably a name
/// collision with a user-owned skill, which the CLI resolves in favour of the
/// user's copy and logs only at debug level.
pub(crate) fn missing_from_init_frame(expected: &[String], reported: &[String]) -> Vec<String> {
    let reported: BTreeSet<&str> = reported.iter().map(String::as_str).collect();
    expected
        .iter()
        .filter(|name| !reported.contains(name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SkillId;

    fn skill(name: &str, dir: &Path) -> ResolvedSkill {
        ResolvedSkill {
            id: SkillId(name.to_string()),
            name: name.to_string(),
            title: None,
            description: Some(format!("{name} description")),
            source_dir: dir.to_path_buf(),
            skill_md_path: dir.join("SKILL.md"),
            body: String::new(),
        }
    }

    /// Materialize and assert no skill was refused, for the tests whose
    /// subject is the resulting root rather than the refusal path.
    fn materialize_ok(parent: &Path, dir_name: &str, skills: &[ResolvedSkill]) -> ClaudeSkillPlugin {
        let (plugin, warnings) =
            ClaudeSkillPlugin::materialize_with_warnings(parent, dir_name, skills)
                .expect("materialize");
        assert!(warnings.is_empty(), "unexpected refusals: {warnings:?}");
        plugin.expect("a plugin")
    }

    fn store_skill(store: &Path, name: &str, skill_md: &str) -> ResolvedSkill {
        let dir = store.join(name);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(dir.join("SKILL.md"), skill_md).expect("write SKILL.md");
        skill(name, &dir)
    }

    #[test]
    fn materialized_root_links_skills_without_copying_bodies() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let body = "---\nname: alpha\ndescription: d\n---\n\nBODYSENTINEL-a1b2c3\n";
        let skills = vec![store_skill(&store, "alpha", body)];

        let plugin = materialize_ok(&runtime, "session-1", &skills);

        assert_eq!(plugin.exposed(), ["tyde-skills:alpha"]);
        let link = plugin.root().join("skills").join("alpha");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink(),
            "skills/alpha must be a symlink, not a copy"
        );
        // The body is reachable through the link but was never copied: the only
        // regular files under the root are the manifest.
        let linked = std::fs::read_to_string(link.join("SKILL.md")).expect("read through link");
        assert!(linked.contains("BODYSENTINEL-a1b2c3"));
        let manifest = std::fs::read_to_string(
            plugin.root().join(".claude-plugin").join("plugin.json"),
        )
        .expect("manifest");
        assert!(manifest.contains("\"name\": \"tyde-skills\""));
        assert!(
            !manifest.contains("\"skills\""),
            "declaring skills shadows the auto-loaded folder: {manifest}"
        );
    }

    #[test]
    fn relative_resources_resolve_through_the_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![store_skill(&store, "guide", "---\nname: guide\n---\nsee art.md\n")];
        std::fs::write(store.join("guide").join("art.md"), "RELSENTINEL-9f8e7d")
            .expect("write sibling resource");

        let plugin = materialize_ok(&runtime, "session-2", &skills);

        let through_link = plugin
            .root()
            .join("skills")
            .join("guide")
            .join("art.md");
        assert_eq!(
            std::fs::read_to_string(through_link).expect("read sibling through link"),
            "RELSENTINEL-9f8e7d",
            "a skill's bundled files must resolve relative to the linked directory"
        );
    }

    #[test]
    fn unaddressable_names_are_refused_visibly() {
        for (name, expected) in [
            ("build:games", "':'"),
            (".hidden", "'.'"),
            ("..", "relative path segment"),
            ("nested/skill", "path separator"),
            ("", "empty"),
        ] {
            let err = skill_name_is_addressable(name).expect_err("must be refused");
            assert!(
                err.contains(expected),
                "name {name:?} refused with {err:?}, expected mention of {expected}"
            );
        }
        skill_name_is_addressable("build-games").expect("ordinary names stay addressable");
    }

    #[test]
    fn refused_names_do_not_silently_vanish() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![
            store_skill(&store, "ok", "---\nname: ok\n---\nbody\n"),
            store_skill(&store, "build:games", "---\nname: x\n---\nbody\n"),
        ];

        let (plugin, warnings) =
            ClaudeSkillPlugin::materialize_with_warnings(&runtime, "session-3", &skills)
                .expect("materialize");

        let plugin = plugin.expect("the addressable skill still materializes");
        assert_eq!(plugin.exposed(), ["tyde-skills:ok"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("build:games") && warnings[0].contains("':'"));
    }

    #[test]
    fn skills_activating_components_are_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![
            store_skill(&store, "inert", "---\nname: inert\ndescription: d\n---\nbody\n"),
            store_skill(
                &store,
                "hooky",
                "---\nname: hooky\nhooks:\n  - event: PreToolUse\n---\nbody\n",
            ),
            store_skill(
                &store,
                "serverish",
                "---\nname: serverish\nmcpServers:\n  x: {}\n---\nbody\n",
            ),
        ];

        let (plugin, warnings) =
            ClaudeSkillPlugin::materialize_with_warnings(&runtime, "session-4", &skills)
                .expect("materialize");

        let plugin = plugin.expect("the inert skill still materializes");
        assert_eq!(plugin.exposed(), ["tyde-skills:inert"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("hooky") && w.contains("hooks")));
        assert!(warnings.iter().any(|w| w.contains("serverish") && w.contains("mcpServers")));
        assert!(
            !plugin.root().join("skills").join("hooky").exists(),
            "a refused skill must not be linked"
        );
    }

    #[test]
    fn a_skill_without_frontmatter_is_still_exposed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![store_skill(&store, "bare", "Just instructions, no frontmatter.\n")];

        let plugin = materialize_ok(&runtime, "session-5", &skills);

        assert_eq!(plugin.exposed(), ["tyde-skills:bare"]);
    }

    #[test]
    fn every_skill_refused_yields_no_plugin_but_keeps_the_reasons() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![store_skill(&store, "build:games", "---\nname: x\n---\nbody\n")];

        let (plugin, warnings) =
            ClaudeSkillPlugin::materialize_with_warnings(&runtime, "session-6", &skills)
                .expect("materialize");

        assert!(plugin.is_none(), "no addressable skill means no plugin root");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            !runtime.join("session-6").exists(),
            "an empty root must not be left behind for the CLI to load"
        );
    }

    #[test]
    fn cleanup_unlinks_only_the_session_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![store_skill(&store, "alpha", "---\nname: alpha\n---\nbody\n")];
        std::fs::write(store.join("alpha").join("art.md"), "keep me").expect("resource");

        let root = {
            let plugin = materialize_ok(&runtime, "session-7", &skills);
            let root = plugin.root().to_path_buf();
            plugin.cleanup().expect("cleanup");
            root
        };

        assert!(!root.exists(), "the session root must be gone");
        assert!(
            store.join("alpha").join("SKILL.md").exists(),
            "cleanup must never follow a link into the skill store"
        );
        assert_eq!(
            std::fs::read_to_string(store.join("alpha").join("art.md")).expect("resource survives"),
            "keep me"
        );
    }

    #[test]
    fn dropping_the_plugin_removes_the_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![store_skill(&store, "alpha", "---\nname: alpha\n---\nbody\n")];

        let root = {
            let plugin = materialize_ok(&runtime, "session-8", &skills);
            plugin.root().to_path_buf()
        };

        assert!(!root.exists(), "drop must unlink the session root");
        assert!(store.join("alpha").join("SKILL.md").exists());
    }

    #[test]
    fn cleanup_refuses_to_delete_a_non_symlink_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![store_skill(&store, "alpha", "---\nname: alpha\n---\nbody\n")];
        let plugin = materialize_ok(&runtime, "session-9", &skills);
        // Something that is not one of our links appears in the farm.
        let intruder = plugin.root().join("skills").join("real-dir");
        std::fs::create_dir_all(&intruder).expect("intruder");
        std::fs::write(intruder.join("keep.txt"), "keep").expect("intruder file");

        let err = plugin.cleanup().expect_err("must refuse");

        assert!(err.contains("not symlinks"), "{err}");
        assert!(
            intruder.join("keep.txt").exists(),
            "an unexpected entry is reported, never recursively deleted"
        );
        std::fs::remove_dir_all(plugin.root()).expect("test teardown");
    }

    #[test]
    fn duplicate_names_collide_visibly_instead_of_overwriting() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store_a = tmp.path().join("a");
        let store_b = tmp.path().join("b");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(&runtime).expect("create runtime");
        let skills = vec![
            store_skill(&store_a, "dup", "---\nname: dup\n---\nfirst\n"),
            store_skill(&store_b, "dup", "---\nname: dup\n---\nsecond\n"),
        ];

        let (plugin, warnings) =
            ClaudeSkillPlugin::materialize_with_warnings(&runtime, "session-10", &skills)
                .expect("materialize");

        let plugin = plugin.expect("the first skill still materializes");
        assert_eq!(plugin.exposed(), ["tyde-skills:dup"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("already claims that name"));
    }

    #[test]
    fn materialize_refuses_an_existing_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir_all(runtime.join("taken")).expect("pre-existing root");
        let skills = vec![store_skill(&store, "alpha", "---\nname: alpha\n---\nbody\n")];

        let err = ClaudeSkillPlugin::materialize_with_warnings(&runtime, "taken", &skills)
            .expect_err("must refuse to reuse a root");

        assert!(err.contains("already exists"), "{err}");
    }

    #[test]
    fn init_frame_verification_names_the_skills_the_cli_dropped() {
        let expected = vec![
            "tyde-skills:alpha".to_string(),
            "tyde-skills:beta".to_string(),
        ];
        let reported = vec![
            "tyde-skills:alpha".to_string(),
            "some-bundled-skill".to_string(),
        ];

        assert_eq!(
            missing_from_init_frame(&expected, &reported),
            ["tyde-skills:beta"]
        );
        assert!(missing_from_init_frame(&expected, &expected).is_empty());
    }

    #[test]
    fn plugin_flag_probe_reads_documented_help_only() {
        assert!(help_text_supports_plugin_dir(
            "  --plugin-dir <path>   Load a plugin from a directory"
        ));
        assert!(!help_text_supports_plugin_dir("  --add-dir <path>"));
        assert_eq!(ClaudePluginFlag::NoMcp.as_str(), "--plugin-dir-no-mcp");
        assert_eq!(ClaudePluginFlag::PluginDir.as_str(), "--plugin-dir");
        assert!(
            unsupported_plugin_dir_error().contains("--plugin-dir"),
            "the error must name the flag, not a version"
        );
    }

    #[test]
    fn only_a_named_flag_rejection_downgrades_the_flag() {
        assert!(stderr_rejects_no_mcp_flag(
            "error: unknown option '--plugin-dir-no-mcp'"
        ));
        assert!(stderr_rejects_no_mcp_flag(
            "Unrecognized option: --plugin-dir-no-mcp"
        ));
        assert!(
            !stderr_rejects_no_mcp_flag("error: unknown option '--frobnicate'"),
            "an unrelated unknown option must not downgrade the plugin flag"
        );
        assert!(
            !stderr_rejects_no_mcp_flag("failed to connect to api.anthropic.com"),
            "an unrelated startup failure must not downgrade the plugin flag"
        );
    }

    #[test]
    fn default_overlay_is_constant_size_and_explicit_overlay_names_skills() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let many: Vec<ResolvedSkill> = (0..25)
            .map(|n| store_skill(&store, &format!("skill{n}"), "---\nname: s\n---\nbody\n"))
            .collect();

        let default_one = native_skill_overlay(SkillSelection::AllInstalled, &many[..1]);
        let default_many = native_skill_overlay(SkillSelection::AllInstalled, &many);
        assert_eq!(
            default_one, default_many,
            "the Default overlay must not grow with the number of installed skills"
        );
        assert!(default_many.contains("tyde-skills:<name>"));
        for skill in &many {
            assert!(
                !default_many.contains(&skill.name),
                "the Default overlay must not enumerate skills"
            );
        }

        let explicit = native_skill_overlay(SkillSelection::Explicit, &many[..2]);
        assert!(explicit.contains("skill0") && explicit.contains("skill1"));
        assert!(explicit.contains("skill0 description"));
        assert!(!explicit.contains("skill2"));
    }

    #[test]
    fn overlays_never_carry_a_skill_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = tmp.path().join("store");
        let skills = vec![store_skill(
            &store,
            "alpha",
            "---\nname: alpha\n---\nBODYSENTINEL-4d5e6f\n",
        )];

        for selection in [SkillSelection::AllInstalled, SkillSelection::Explicit] {
            let overlay = native_skill_overlay(selection, &skills);
            assert!(
                !overlay.contains("BODYSENTINEL-4d5e6f"),
                "{selection:?} overlay leaked a body: {overlay}"
            );
            assert!(
                !overlay.contains("Skill: "),
                "{selection:?} overlay emitted a legacy body block: {overlay}"
            );
        }
    }

    #[test]
    fn frontmatter_extraction_tolerates_shapes_that_are_not_activation() {
        assert!(refused_keys_in_frontmatter("no frontmatter here\n").is_empty());
        assert!(refused_keys_in_frontmatter("---\nname: a\ndescription: b\n---\nbody\n").is_empty());
        assert!(refused_keys_in_frontmatter("---\n: : not yaml : :\n---\nbody\n").is_empty());
        assert!(
            refused_keys_in_frontmatter("---\nname: a\n---\n").is_empty(),
            "frontmatter with an empty body is still parsed"
        );
        assert_eq!(
            refused_keys_in_frontmatter("---\nname: a\nHooks:\n  - x\n---\nbody\n"),
            ["Hooks"],
            "activation keys are matched case-insensitively and reported as written"
        );
        assert!(
            refused_keys_in_frontmatter("---\nname: a\n---\nbody with hooks: in it\n").is_empty(),
            "only frontmatter keys count, not body text"
        );
    }
}
