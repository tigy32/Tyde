//! Shared skill projection: turn selected Tyde-store skills into a directory
//! tree that a backend's own loader discovers.
//!
//! Every backend with native skill discovery needs the same thing — a private,
//! session-owned wrapper directory per selected skill:
//!
//! ```text
//! <skills dir>/<name>/
//! ├── SKILL.md                      generated snapshot, 0600
//! └── <resource> -> <store>/<name>/<resource>   symlink, non-SKILL.md only
//! ```
//!
//! `SKILL.md` is **not** symlinked. The store's bytes are read exactly once,
//! validated, and a semantic snapshot is written from those same bytes, so what
//! the backend loads is what Tyde inspected: there is no window in which the
//! file can change between validation and load. Everything else in the skill
//! directory is symlinked, so a skill's relative assets and scripts still
//! resolve without copying the whole store per session.
//!
//! What differs between backends is only *policy*: which top-level resource
//! names would collide with that backend's own configuration surface, and
//! whether its loader tolerates a skill with no description. That is
//! [`ProjectionPolicy`]; the rest is shared.
//!
//! Tyde never writes the user's backend configuration or skill store. It writes
//! only inside its own private per-session root, and reads the store read-only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::agent::customization::ResolvedSkill;

/// Upper bound on a synthesized skill name.
///
/// Skill names become directory names and frontmatter values. 64 is comfortably
/// under every path-component limit Tyde needs to survive, short enough to stay
/// readable in a model's skill listing, and is also the limit Tycode's own
/// loader enforces.
pub(crate) const EXPOSED_NAME_MAX_LEN: usize = 64;

/// Why one selected skill was not exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRefusal {
    pub(crate) name: String,
    pub(crate) reason: String,
}

impl SkillRefusal {
    pub(crate) fn describe(&self) -> String {
        format!("skill '{}' was not exposed: {}", self.name, self.reason)
    }
}

/// Whether the target loader can load a skill that declares no description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DescriptionPolicy {
    /// A missing description is passed through as missing. Claude Code loads a
    /// `SKILL.md` with no frontmatter at all, so inventing text there would put
    /// words in a skill author's mouth for no gain.
    Optional,
    /// The loader rejects a skill whose frontmatter has no non-empty
    /// `description`, so one is always synthesized. Tycode's parser is this:
    /// without a description it refuses the file with only a log warning, which
    /// would make the skill vanish silently.
    Required,
}

/// The per-backend part of projecting a skill.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjectionPolicy {
    /// Top-level resource names never linked into a wrapper, because they name
    /// an activation or configuration surface of the target backend rather than
    /// skill content. A skill shipping one is refused rather than silently
    /// stripped.
    pub(crate) refused_resource_names: &'static [&'static str],
    pub(crate) description: DescriptionPolicy,
}

/// One skill that was successfully projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectedSkill {
    /// Synthesized wrapper name: the directory name, the frontmatter `name`,
    /// and the name the model addresses. Safe by construction and unique within
    /// the session.
    pub(crate) name: String,
    /// The store directory name the user knows this skill by. Shown alongside
    /// the exposed name when the two differ, so a renamed skill is never a
    /// mystery.
    pub(crate) source_name: String,
    /// One-line description for the backend's catalog.
    pub(crate) description: Option<String>,
}

/// One skill that passed inspection: the bytes to write and the links to make.
pub(crate) struct InspectedSkill {
    pub(crate) projected: ProjectedSkill,
    /// Store directory name, kept so a later failure can name the skill the way
    /// the user knows it rather than by its synthesized wrapper name.
    pub(crate) source_name: String,
    contents: String,
    /// `(link name, target)` for each top-level resource, `SKILL.md` excluded.
    resources: Vec<(String, PathBuf)>,
}

/// Validate one skill and produce the exact `SKILL.md` bytes the backend loads.
///
/// Every reason a skill cannot be exposed is discovered here, before anything is
/// written, so the caller sees a refusal rather than a partially built root.
pub(crate) fn inspect_skill(
    skill: &ResolvedSkill,
    claimed: &mut BTreeSet<String>,
    policy: ProjectionPolicy,
) -> Result<InspectedSkill, String> {
    // One read. These bytes are what gets validated and what gets written, so
    // a later rewrite of the store file cannot change what the backend loads.
    let raw = std::fs::read_to_string(&skill.skill_md_path)
        .map_err(|err| format!("could not read {}: {err}", skill.skill_md_path.display()))?;
    let ParsedSkillMd {
        mut frontmatter,
        description: frontmatter_description,
        body,
    } = parse_skill_md(&raw)?;
    let resources = inspect_resources(skill, policy.refused_resource_names)?;
    let ResolvedDescription {
        catalog: description,
        synthesized,
    } = resolve_description(skill, frontmatter_description, policy.description);
    // Only a *synthesized* description is written. A skill that declares its own
    // keeps it byte for byte: the frontmatter is the author's semantic payload,
    // and rewriting a multi-line description into a collapsed one-liner would
    // change what they wrote to suit a catalog line Tyde keeps separately.
    if let Some(synthesized) = synthesized {
        frontmatter.insert(
            serde_yaml::Value::String("description".to_string()),
            serde_yaml::Value::String(synthesized),
        );
    }
    // Synthesized last, and only for a skill that is otherwise going to be
    // exposed, so a refused skill never consumes an ordinal and the numbering
    // stays a function of what actually materializes.
    let exposed = synthesize_exposed_name(&skill.name, claimed);
    let contents = render_skill_md(&exposed, frontmatter, body)?;
    Ok(InspectedSkill {
        projected: ProjectedSkill {
            name: exposed,
            source_name: skill.name.clone(),
            description,
        },
        source_name: skill.name.clone(),
        contents,
        resources,
    })
}

/// The description a skill is catalogued under, and the one that has to be
/// written into its wrapper.
///
/// These are deliberately different values. `catalog` is a collapsed one-liner
/// for a listing; `synthesized` is `Some` only when the source declared nothing
/// usable and the wrapper must gain a `description` key it did not have.
struct ResolvedDescription {
    catalog: Option<String>,
    synthesized: Option<String>,
}

/// Settle on the description the wrapper declares.
///
/// A source description always wins and is never rewritten. Beyond that the two
/// policies differ only in what happens when there is nothing to inherit:
/// `Optional` leaves it absent, `Required` falls back through the store's
/// metadata to the skill's own name — the weakest claim that is still true and
/// still loads.
fn resolve_description(
    skill: &ResolvedSkill,
    frontmatter_description: Option<String>,
    policy: DescriptionPolicy,
) -> ResolvedDescription {
    let non_empty = |text: &str| {
        let collapsed = collapse_to_one_line(text);
        (!collapsed.is_empty()).then_some(collapsed)
    };
    if let Some(catalog) = frontmatter_description.as_deref().and_then(non_empty) {
        return ResolvedDescription {
            catalog: Some(catalog),
            synthesized: None,
        };
    }
    let synthesized = skill
        .description
        .as_deref()
        .and_then(non_empty)
        .or_else(|| match policy {
            DescriptionPolicy::Optional => None,
            DescriptionPolicy::Required => skill
                .title
                .as_deref()
                .and_then(non_empty)
                .or_else(|| non_empty(&skill.name)),
        });
    ResolvedDescription {
        catalog: synthesized.clone(),
        synthesized,
    }
}

/// Write one wrapper. Every failure here names a single skill.
pub(crate) fn write_wrapper(skills_dir: &Path, entry: &InspectedSkill) -> Result<(), String> {
    let wrapper = skills_dir.join(&entry.projected.name);
    // Exclusive, not `create_dir_all`. If two skills ever did land on the same
    // wrapper name — a bug in synthesis, or a filesystem that considers two
    // distinct names equal — the second would otherwise overwrite the first's
    // SKILL.md silently. Failing here turns that into a visible per-skill
    // refusal instead.
    std::fs::create_dir(&wrapper).map_err(|err| {
        format!(
            "its wrapper directory {} could not be created exclusively: {err}",
            wrapper.display()
        )
    })?;
    restrict_dir(&wrapper)?;
    write_private_file(&wrapper.join("SKILL.md"), &entry.contents)?;
    for (link_name, target) in &entry.resources {
        symlink_path(target, &wrapper.join(link_name))
            .map_err(|err| format!("its resource '{link_name}' could not be linked: {err}"))?;
    }
    Ok(())
}

/// Remove a wrapper that failed halfway through, so the backend never sees half
/// a skill. The root itself stays valid for the others.
pub(crate) fn discard_wrapper(skills_dir: &Path, name: &str) {
    let _ = std::fs::remove_dir_all(skills_dir.join(name));
}

/// Enumerate and validate the top-level entries that will be linked.
fn inspect_resources(
    skill: &ResolvedSkill,
    refused_resource_names: &[&str],
) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = std::fs::read_dir(&skill.source_dir).map_err(|err| {
        format!(
            "could not list its resources at {}: {err}",
            skill.source_dir.display()
        )
    })?;
    let canonical_source = skill
        .source_dir
        .canonicalize()
        .map_err(|err| format!("its directory could not be resolved: {err}"))?;
    let mut resources = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("one of its resources could not be read: {err}"))?;
        let file_name = entry.file_name();
        let Some(as_str) = file_name.to_str() else {
            return Err("it has a resource whose name is not valid UTF-8".to_string());
        };
        if as_str == "SKILL.md" {
            // Never linked: the wrapper's SKILL.md is Tyde's inspected snapshot.
            continue;
        }
        if refused_resource_names.contains(&as_str) {
            return Err(format!(
                "it ships a top-level '{as_str}', which names a backend activation surface and \
                 must not appear in a Tyde skill wrapper"
            ));
        }
        // Defence in depth: a resource that already escapes its own skill
        // directory is refused rather than linked. The link Tyde creates still
        // resolves at load time, so this narrows the surface without pretending
        // to be a containment proof — only copying would be that.
        let resolved = entry
            .path()
            .canonicalize()
            .map_err(|err| format!("its resource '{as_str}' could not be resolved: {err}"))?;
        if !resolved.starts_with(&canonical_source) {
            return Err(format!(
                "its resource '{as_str}' resolves outside its own directory"
            ));
        }
        // Link the path that was *checked*, not the entry path. `entry.path()`
        // may itself be a symlink, and re-resolving it at link time would mean
        // the wrapper points at whatever it resolves to then rather than at the
        // directory this containment check just approved.
        resources.push((as_str.to_string(), resolved));
    }
    resources.sort();
    Ok(resources)
}

pub(crate) struct ParsedSkillMd<'a> {
    pub(crate) frontmatter: serde_yaml::Mapping,
    pub(crate) description: Option<String>,
    /// Instructional body, preserved exactly as read.
    pub(crate) body: &'a str,
}

/// Split and validate a `SKILL.md`.
///
/// Fail-closed on malformed frontmatter: an unterminated block, YAML that does
/// not parse, or YAML that is not a mapping is a refusal, not something to
/// guess around. Empty, comment-only, and YAML-null frontmatter declare an empty
/// mapping. A file with **no** frontmatter at all is also fine — Claude 2.1.220
/// loads one — and its whole content is the body.
pub(crate) fn parse_skill_md(raw: &str) -> Result<ParsedSkillMd<'_>, String> {
    let Some(rest) = strip_frontmatter_open(raw) else {
        return Ok(ParsedSkillMd {
            frontmatter: serde_yaml::Mapping::new(),
            description: None,
            body: raw,
        });
    };
    let Some((frontmatter, body)) = split_frontmatter_close(rest) else {
        return Err(
            "its SKILL.md opens a '---' frontmatter block that is never closed".to_string(),
        );
    };
    let value: serde_yaml::Value = serde_yaml::from_str(frontmatter)
        .map_err(|err| format!("its SKILL.md frontmatter is not valid YAML: {err}"))?;
    let mapping = match value {
        serde_yaml::Value::Mapping(mapping) => mapping,
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        _ => return Err("its SKILL.md frontmatter is not a YAML mapping".to_string()),
    };

    let description = validate_optional_string_field(&mapping, "description")?;
    Ok(ParsedSkillMd {
        frontmatter: mapping,
        description,
        body,
    })
}

fn validate_optional_string_field(
    mapping: &serde_yaml::Mapping,
    field: &str,
) -> Result<Option<String>, String> {
    let Some(value) = mapping.get(field) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|text| Some(text.to_string()))
        .ok_or_else(|| format!("its SKILL.md frontmatter field '{field}' must be a YAML string"))
}

fn strip_frontmatter_open(raw: &str) -> Option<&str> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    raw.strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
}

fn split_frontmatter_close(rest: &str) -> Option<(&str, &str)> {
    if rest == "---" {
        return Some(("", ""));
    }
    if let Some(body) = rest.strip_prefix("---\n") {
        return Some(("", body));
    }
    if let Some(body) = rest.strip_prefix("---\r\n") {
        return Some(("", body));
    }
    if let Some(index) = rest.find("\n---\n") {
        return Some((&rest[..index], &rest[index + 5..]));
    }
    if let Some(index) = rest.find("\n---\r\n") {
        return Some((&rest[..index], &rest[index + 6..]));
    }
    // A frontmatter block that ends the file with no trailing newline.
    if let Some(frontmatter) = rest.strip_suffix("\n---") {
        return Some((frontmatter, ""));
    }
    None
}

/// Build the semantic snapshot: complete frontmatter plus the body as read.
///
/// `name` must equal the wrapper directory name so the skill is addressed the
/// same way whichever of the two the loader keys on — Claude keys on the
/// directory, Tycode keys on the frontmatter. Every case/whitespace variant of
/// that key is removed before the authoritative lowercase key is inserted, so
/// source YAML cannot supersede or compete with the synthesized name.
///
/// The body is appended **byte for byte**. Nothing is trimmed: leading blank
/// lines, indentation, and trailing whitespace are all part of a Markdown
/// document's meaning, and a wrapper that quietly reflowed a skill would be
/// changing instructions the user wrote.
pub(crate) fn render_skill_md(
    name: &str,
    mut frontmatter: serde_yaml::Mapping,
    body: &str,
) -> Result<String, String> {
    frontmatter.retain(|key, _| !is_wrapper_name_key(key));
    frontmatter.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(name.to_string()),
    );
    let canonical = canonicalize_yaml(serde_yaml::Value::Mapping(frontmatter))?;
    let rendered = serde_yaml::to_string(&canonical)
        .map_err(|err| format!("its SKILL.md frontmatter could not be serialized: {err}"))?;
    Ok(format!("---\n{rendered}---\n{body}"))
}

pub(crate) fn is_wrapper_name_key(key: &serde_yaml::Value) -> bool {
    key.as_str()
        .is_some_and(|key| key.trim().eq_ignore_ascii_case("name"))
}

fn canonicalize_yaml(value: serde_yaml::Value) -> Result<serde_yaml::Value, String> {
    match value {
        serde_yaml::Value::Sequence(values) => values
            .into_iter()
            .map(canonicalize_yaml)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_yaml::Value::Sequence),
        serde_yaml::Value::Mapping(mapping) => {
            let mut entries = Vec::with_capacity(mapping.len());
            for (key, value) in mapping {
                let key = canonicalize_yaml(key)?;
                let value = canonicalize_yaml(value)?;
                let sort_key = serde_yaml::to_string(&key).map_err(|err| {
                    format!("its SKILL.md frontmatter key could not be serialized: {err}")
                })?;
                entries.push((sort_key, key, value));
            }
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_yaml::Mapping::with_capacity(entries.len());
            for (_, key, value) in entries {
                canonical.insert(key, value);
            }
            Ok(serde_yaml::Value::Mapping(canonical))
        }
        serde_yaml::Value::Tagged(mut tagged) => {
            tagged.value = canonicalize_yaml(tagged.value)?;
            Ok(serde_yaml::Value::Tagged(tagged))
        }
        scalar => Ok(scalar),
    }
}

/// Derive a wrapper name that is safe to use as both a directory name and a
/// backend-addressable skill name, and that cannot be confused with another one.
///
/// Source names come from the user's store and are only guaranteed to be
/// directory names. On a case-insensitive or Unicode-normalizing filesystem
/// `Build-Games`, `build-games`, and a decomposed-accent variant can all be the
/// same directory, so using them verbatim would let one wrapper silently
/// overwrite another. Synthesis maps every source name into a restricted ASCII
/// alphabet and disambiguates by **ordinal suffix**, deterministically, in
/// selection order.
///
/// The alphabet is also what makes a store name a *loadable* name: Tycode's
/// parser rejects anything outside `[a-z0-9-]`, so a store skill named
/// `TycodeGames` is only discoverable at all because synthesis lowercases it.
pub(crate) fn synthesize_exposed_name(source: &str, claimed: &mut BTreeSet<String>) -> String {
    let mut base: String = source
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                // Everything else, including `_` and every non-ASCII character,
                // becomes `-`. The alphabet is exactly [a-z0-9-]: anything
                // wider risks a name the backend, the filesystem, or a
                // namespace treats differently from how Tyde does.
                '-'
            }
        })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    base = base.trim_matches('-').to_string();
    if base.is_empty() || base.starts_with(|ch: char| ch.is_ascii_digit()) {
        base = format!("skill-{base}");
        base = base.trim_end_matches('-').to_string();
    }
    base = truncate_to_limit(&base, EXPOSED_NAME_MAX_LEN);
    if claimed.insert(collision_key(&base)) {
        return base;
    }
    // Deterministic: the second `build-games` becomes `build-games-2`, the
    // third `build-games-3`, in the order the selection listed them. The suffix
    // is part of the budget, so a name at the limit is shortened to make room
    // rather than pushed over it.
    for ordinal in 2u32.. {
        let suffix = format!("-{ordinal}");
        let room = EXPOSED_NAME_MAX_LEN.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", truncate_to_limit(&base, room));
        if claimed.insert(collision_key(&candidate)) {
            return candidate;
        }
    }
    unreachable!("an ordinal suffix always terminates")
}

pub(crate) fn collapse_to_one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The key two exposed names are compared under.
///
/// Names are already a conservative ASCII slug — `[a-z0-9-]` — so this is an
/// identity today. It exists as a named step so the equivalence being enforced
/// is explicit rather than an emergent property of the alphabet: two sources
/// that differ only by case, by Unicode normal form, or by any character the
/// slug folds to `-` must land on the same key and therefore be given distinct
/// names. If the alphabet is ever widened, this is the one place that has to
/// grow a real casefold/NFC step with it.
fn collision_key(name: &str) -> String {
    name.to_lowercase()
}

/// Cut to `limit` characters without leaving a trailing separator.
fn truncate_to_limit(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let cut = value[..limit].trim_end_matches('-').to_string();
    if cut.is_empty() {
        "skill".to_string()
    } else {
        cut
    }
}

/// Create a private session root, or fail visibly.
///
/// On unix this is a `TempDir` restricted to `0700`. On any other platform Tyde
/// cannot currently prove the root is private or that resource links can be
/// created, so it refuses **before the backend process starts** rather than
/// materializing a world-readable skill tree and hoping. A visible "not
/// supported here" beats a silent insecure fallback.
pub(crate) fn create_private_root(
    parent: Option<&Path>,
    prefix: &str,
) -> Result<tempfile::TempDir, String> {
    let guard = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(
            parent
                .map(Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir),
        )
        .map_err(|err| format!("Failed to create a private skill root: {err}"))?;
    restrict_dir(guard.path())?;
    Ok(guard)
}

pub(crate) fn create_private_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|err| format!("Failed to create {}: {err}", path.display()))?;
    restrict_dir(path)
}

pub(crate) fn write_private_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))?;
    restrict_file(path)
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("Failed to restrict {}: {err}", path.display()))
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("Failed to restrict {}: {err}", path.display()))
}

/// On a platform where Tyde cannot make the root private, refuse.
///
/// These are deliberately hard errors rather than no-ops. A no-op would leave a
/// root containing the user's skill instructions readable by every other account
/// on the machine, and would do so silently. Failing here happens **before the
/// backend process starts**, so the session reports "skills are not supported on
/// this platform" instead of quietly running insecurely.
#[cfg(not(unix))]
fn restrict_dir(path: &Path) -> Result<(), String> {
    Err(unsupported_platform_error(path))
}

#[cfg(not(unix))]
fn restrict_file(path: &Path) -> Result<(), String> {
    Err(unsupported_platform_error(path))
}

#[cfg(not(unix))]
fn unsupported_platform_error(path: &Path) -> String {
    format!(
        "Tyde cannot create a private skill root at {} on this platform: restricting it to the \
         current user is not implemented here, and Tyde will not materialize skill instructions \
         into a world-readable directory. Skills are unavailable until this platform gains a \
         secure temporary-directory path.",
        path.display()
    )
}

#[cfg(unix)]
pub(crate) fn symlink_path(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub(crate) fn symlink_path(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
