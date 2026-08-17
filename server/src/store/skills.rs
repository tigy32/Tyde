use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use protocol::{Skill, SkillId};
use serde::{Deserialize, Serialize};

const SKILL_METADATA_FILENAME: &str = "metadata.json";
const SKILL_BODY_FILENAME: &str = "SKILL.md";

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<String, Skill>,
}

#[derive(Debug, Clone)]
pub struct SkillSyncResult {
    pub upserts: Vec<Skill>,
    pub deletes: Vec<SkillId>,
}

/// Canonical on-disk locations for one installed skill.
///
/// Backends that discover skills natively are handed these paths instead of a
/// body, so both are canonicalized and proven to stay inside the store root
/// before they leave `SkillStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPaths {
    /// The skill's directory, `<root_dir>/<name>` resolved through symlinks.
    pub source_dir: PathBuf,
    /// `<source_dir>/SKILL.md`.
    pub skill_md: PathBuf,
}

#[derive(Debug)]
pub struct SkillStore {
    index_path: PathBuf,
    root_dir: PathBuf,
}

impl SkillStore {
    pub fn load(index_path: PathBuf, root_dir: PathBuf) -> Result<Self, String> {
        let store = Self {
            index_path,
            root_dir,
        };
        let _ = store.read_or_rebuild_index()?;
        Ok(store)
    }

    pub fn default_index_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SKILLS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?.join(".tyde").join("skills.json"))
    }

    pub fn default_root_dir() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SKILLS_DIR_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?.join(".tyde").join("skills"))
    }

    pub fn list(&self) -> Result<Vec<Skill>, String> {
        let mut skills = self
            .read_or_rebuild_index()?
            .into_values()
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(skills)
    }

    pub fn get(&self, id: &SkillId) -> Option<Skill> {
        self.read_or_rebuild_index()
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    /// Resolve the canonical directory and `SKILL.md` path for `id`.
    ///
    /// Each path is canonicalized and contained independently: the directory
    /// must resolve inside the store root, and `SKILL.md` must resolve inside
    /// that directory and be a regular file. Deriving the file path from the
    /// checked directory is not enough — `<skill>/SKILL.md` can itself be a
    /// symlink out of the store, or a directory or device node, and a backend
    /// handed the path would follow it.
    pub fn skill_paths(&self, id: &SkillId) -> Result<SkillPaths, String> {
        let skill = self
            .get(id)
            .ok_or_else(|| format!("cannot resolve missing skill {}", id))?;
        let source_dir = canonical_within(&self.root_dir, &self.root_dir.join(&skill.name))
            .map_err(|err| format!("cannot resolve skill {} directory: {err}", skill.id))?;
        let skill_md = canonical_within(&source_dir, &source_dir.join(SKILL_BODY_FILENAME))
            .map_err(|err| {
                format!(
                    "cannot resolve skill {} {SKILL_BODY_FILENAME}: {err}",
                    skill.id
                )
            })?;
        let metadata = std::fs::metadata(&skill_md)
            .map_err(|err| format!("Failed to stat {}: {err}", skill_md.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "skill {} {} is not a regular file",
                skill.id,
                skill_md.display()
            ));
        }
        Ok(SkillPaths {
            source_dir,
            skill_md,
        })
    }

    pub fn load_body(&self, id: &SkillId) -> Result<String, String> {
        let path = self.skill_paths(id)?.skill_md;
        std::fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read skill body {}: {err}", path.display()))
    }

    pub fn upsert(&self, skill: Skill, body: String) -> Result<Skill, String> {
        validate_skill(&skill)?;
        if body.trim().is_empty() {
            return Err(format!("skill {} body must not be empty", skill.id));
        }

        let mut records = self.read_or_rebuild_index()?;
        if let Some(existing) = records.get(&skill.id.0)
            && existing.name != skill.name
        {
            return Err(format!(
                "cannot change skill {} directory name from '{}' to '{}'",
                skill.id, existing.name, skill.name
            ));
        }
        if let Some(existing) = records
            .values()
            .find(|existing| existing.name == skill.name && existing.id != skill.id)
        {
            return Err(format!(
                "cannot upsert skill {} with duplicate directory name '{}' already used by {}",
                skill.id, skill.name, existing.id
            ));
        }

        let skill_dir = self.root_dir.join(&skill.name);
        std::fs::create_dir_all(&skill_dir).map_err(|err| {
            format!(
                "Failed to create skill directory {}: {err}",
                skill_dir.display()
            )
        })?;
        write_atomic(
            &skill_dir.join(SKILL_METADATA_FILENAME),
            serde_json::to_string_pretty(&skill)
                .map_err(|err| format!("Failed to serialize skill metadata: {err}"))?
                .as_bytes(),
        )?;
        write_atomic(&skill_dir.join(SKILL_BODY_FILENAME), body.as_bytes())?;

        records.insert(skill.id.0.clone(), skill.clone());
        self.save_index(&records)?;
        Ok(skill)
    }

    pub fn delete(&self, id: &SkillId) -> Result<SkillId, String> {
        let mut records = self.read_or_rebuild_index()?;
        let skill = records
            .remove(&id.0)
            .ok_or_else(|| format!("cannot delete missing skill {}", id))?;
        let skill_dir = self.root_dir.join(&skill.name);
        match std::fs::remove_dir_all(&skill_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to delete skill directory {}: {err}",
                    skill_dir.display()
                ));
            }
        }
        self.save_index(&records)?;
        Ok(id.clone())
    }

    pub fn sync_from_disk(&self) -> Result<SkillSyncResult, String> {
        let previous = self.read_or_rebuild_index()?;
        let next = self.scan_disk()?;
        self.save_index(&next)?;

        let mut upserts = Vec::new();
        for (id, skill) in &next {
            match previous.get(id) {
                Some(previous_skill) if previous_skill == skill => {}
                Some(_) | None => upserts.push(skill.clone()),
            }
        }
        upserts.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));

        let mut deletes = previous
            .keys()
            .filter(|id| !next.contains_key(*id))
            .map(|id| SkillId(id.clone()))
            .collect::<Vec<_>>();
        deletes.sort_by(|left, right| left.0.cmp(&right.0));

        Ok(SkillSyncResult { upserts, deletes })
    }

    fn read_or_rebuild_index(&self) -> Result<HashMap<String, Skill>, String> {
        if !self.index_path.is_file() {
            let records = self.scan_disk()?;
            self.save_index(&records)?;
            return Ok(records);
        }
        match self.read_index() {
            Ok(records) => Ok(records),
            Err(err) => {
                tracing::warn!(
                    index = %self.index_path.display(),
                    root_dir = %self.root_dir.display(),
                    error = %err,
                    "skills index invalid; rebuilding from disk"
                );
                let records = self.scan_disk()?;
                self.save_index(&records)?;
                Ok(records)
            }
        }
    }

    fn read_index(&self) -> Result<HashMap<String, Skill>, String> {
        match std::fs::read_to_string(&self.index_path) {
            Ok(contents) => {
                let records = serde_json::from_str::<StoreFile>(&contents)
                    .map(|store| store.records)
                    .map_err(|err| {
                        format!(
                            "Failed to parse skills index {}: {err}",
                            self.index_path.display()
                        )
                    })?;
                for skill in records.values() {
                    validate_skill(skill).map_err(|err| {
                        format!("Invalid skills index {}: {err}", self.index_path.display())
                    })?;
                }
                Ok(records)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read skills index {}: {err}",
                self.index_path.display()
            )),
        }
    }

    fn scan_disk(&self) -> Result<HashMap<String, Skill>, String> {
        let mut dir_names = Vec::new();
        match std::fs::read_dir(&self.root_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(err) => {
                            tracing::warn!(
                                root_dir = %self.root_dir.display(),
                                error = %err,
                                "failed to read skill directory entry; skipping"
                            );
                            continue;
                        }
                    };
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(err) => {
                            tracing::warn!(
                                path = %entry.path().display(),
                                error = %err,
                                "failed to stat skill directory entry; skipping"
                            );
                            continue;
                        }
                    };
                    if !file_type.is_dir() {
                        continue;
                    }

                    dir_names.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to read skills directory {}: {err}",
                    self.root_dir.display()
                ));
            }
        }

        // `read_dir` yields entries in an unspecified order, so two directories
        // whose `metadata.json` claims the same id used to resolve to whichever
        // one the filesystem happened to return last. Sort first and keep the
        // first directory by name, so the surviving skill is the same on every
        // scan and on every machine.
        dir_names.sort();

        let mut records: HashMap<String, Skill> = HashMap::new();
        for dir_name in dir_names {
            let path = self.root_dir.join(&dir_name);
            let Some(skill) = load_skill_from_dir(&path, &dir_name) else {
                continue;
            };
            match records.entry(skill.id.0.clone()) {
                std::collections::hash_map::Entry::Occupied(occupied) => {
                    tracing::warn!(
                        skill_id = %skill.id,
                        kept = %occupied.get().name,
                        skipped = %dir_name,
                        path = %path.display(),
                        "duplicate skill id on disk; keeping the first directory by name"
                    );
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(skill);
                }
            }
        }
        Ok(records)
    }

    fn save_index(&self, records: &HashMap<String, Skill>) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            records: records.clone(),
        })
        .map_err(|err| format!("Failed to serialize skills index: {err}"))?;

        let parent = self.index_path.parent().ok_or_else(|| {
            format!(
                "Skills index path has no parent: {}",
                self.index_path.display()
            )
        })?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create skills index directory: {err}"))?;
        std::fs::create_dir_all(&self.root_dir)
            .map_err(|err| format!("Failed to create skills root directory: {err}"))?;

        let tmp_path = self.index_path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp skills index file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp skills index file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp skills index file: {err}"))?;
        std::fs::rename(&tmp_path, &self.index_path).map_err(|err| {
            format!(
                "Failed to atomically replace skills index {}: {err}",
                self.index_path.display()
            )
        })?;
        Ok(())
    }
}

/// Canonicalize `candidate` and prove it stays inside `root`.
///
/// Both sides are canonicalized so `..` segments and symlinks are resolved
/// before the comparison. `Path::starts_with` then matches whole components, so
/// a sibling such as `<root>-elsewhere` is rejected rather than mistaken for a
/// nested path.
fn canonical_within(
    root: &std::path::Path,
    candidate: &std::path::Path,
) -> Result<PathBuf, String> {
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|err| format!("Failed to resolve {}: {err}", root.display()))?;
    let canonical_candidate = std::fs::canonicalize(candidate)
        .map_err(|err| format!("Failed to resolve {}: {err}", candidate.display()))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(format!(
            "{} resolves to {}, which is outside {}",
            candidate.display(),
            canonical_candidate.display(),
            canonical_root.display()
        ));
    }
    Ok(canonical_candidate)
}

fn write_atomic(path: &std::path::Path, contents: &[u8]) -> Result<(), String> {
    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("tmp")
    ));
    let mut file = std::fs::File::create(&tmp_path)
        .map_err(|err| format!("Failed to create temp file {}: {err}", tmp_path.display()))?;
    file.write_all(contents)
        .map_err(|err| format!("Failed to write temp file {}: {err}", tmp_path.display()))?;
    file.sync_all()
        .map_err(|err| format!("Failed to sync temp file {}: {err}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|err| {
        format!(
            "Failed to atomically replace {} with {}: {err}",
            path.display(),
            tmp_path.display()
        )
    })
}

fn load_skill_from_dir(path: &std::path::Path, dir_name: &str) -> Option<Skill> {
    let metadata_path = path.join(SKILL_METADATA_FILENAME);
    let body_path = path.join(SKILL_BODY_FILENAME);

    if !body_path.is_file() {
        tracing::warn!(
            path = %path.display(),
            "skill directory is missing {}; skipping",
            SKILL_BODY_FILENAME
        );
        return None;
    }

    let skill = if metadata_path.is_file() {
        let contents = match std::fs::read_to_string(&metadata_path) {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!(
                    path = %metadata_path.display(),
                    error = %err,
                    "failed to read skill metadata; skipping"
                );
                return None;
            }
        };
        match serde_json::from_str::<Skill>(&contents) {
            Ok(skill) => skill,
            Err(err) => {
                tracing::warn!(
                    path = %metadata_path.display(),
                    error = %err,
                    "failed to parse skill metadata; skipping"
                );
                return None;
            }
        }
    } else {
        Skill {
            id: SkillId(dir_name.to_string()),
            name: dir_name.to_string(),
            title: None,
            description: None,
        }
    };

    if let Err(err) = validate_skill(&skill) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "invalid skill metadata; skipping"
        );
        return None;
    }
    if skill.name != dir_name {
        tracing::warn!(
            path = %metadata_path.display(),
            skill_name = %skill.name.as_str(),
            dir_name = %dir_name,
            "skill metadata name does not match directory name; skipping"
        );
        return None;
    }

    Some(skill)
}

fn validate_skill(skill: &Skill) -> Result<(), String> {
    if skill.id.0.trim().is_empty() {
        return Err("skill id must not be empty".to_string());
    }
    if skill.name.trim().is_empty() {
        return Err(format!("skill {} name must not be empty", skill.id));
    }
    // Separators and the two relative path segments are rejected, because
    // `name` is joined onto the store root by the mutating paths: `upsert`
    // writes into `<root>/<name>` and `delete` calls `remove_dir_all` on it.
    // `.` would target the store root itself and `..` its parent — for a
    // default store, the whole of `~/.tyde`. No real skill can be named either
    // one, since a directory scan never yields them, so rejecting them drops
    // nothing a user has.
    if skill.name.contains(std::path::MAIN_SEPARATOR)
        || skill.name.contains('/')
        || skill.name.contains('\\')
    {
        return Err(format!(
            "skill {} name '{}' must be a single directory name",
            skill.id, skill.name
        ));
    }
    if skill.name == "." || skill.name == ".." {
        return Err(format!(
            "skill {} name '{}' must be a directory name, not a relative path segment",
            skill.id, skill.name
        ));
    }
    // Anything else is left alone. A name a native skill loader dislikes — a
    // leading `.`, or a `:` that collides with plugin namespacing — is still a
    // skill the user installed and has been using: rejecting it would make an
    // upgrade silently drop it from `list()` and from every session. Whether
    // such a name can be exposed to a given backend is that adapter's call,
    // per session, and must be normalized or reported there.
    if skill
        .title
        .as_ref()
        .is_some_and(|title| title.trim().is_empty())
    {
        return Err(format!(
            "skill {} title must not be blank when provided",
            skill.id
        ));
    }
    if skill
        .description
        .as_ref()
        .is_some_and(|description| description.trim().is_empty())
    {
        return Err(format!(
            "skill {} description must not be blank when provided",
            skill.id
        ));
    }
    Ok(())
}
