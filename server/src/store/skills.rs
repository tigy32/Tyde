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
        let _ = store.read_index()?;
        Ok(store)
    }

    pub fn default_index_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SKILLS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("skills.json"))
    }

    pub fn default_root_dir() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_SKILLS_DIR_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("skills"))
    }

    pub fn list(&self) -> Result<Vec<Skill>, String> {
        let mut skills = self.read_index()?.into_values().collect::<Vec<_>>();
        skills.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.0.cmp(&right.id.0)));
        Ok(skills)
    }

    pub fn get(&self, id: &SkillId) -> Option<Skill> {
        self.read_index()
            .ok()
            .and_then(|records| records.get(&id.0).cloned())
    }

    pub fn load_body(&self, id: &SkillId) -> Result<String, String> {
        let skill = self
            .get(id)
            .ok_or_else(|| format!("cannot resolve missing skill {}", id))?;
        let path = self.root_dir.join(&skill.name).join(SKILL_BODY_FILENAME);
        std::fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read skill body {}: {err}", path.display()))
    }

    pub fn sync_from_disk(&self) -> Result<SkillSyncResult, String> {
        let previous = self.read_index()?;
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
        let mut records = HashMap::new();
        match std::fs::read_dir(&self.root_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|err| {
                        format!(
                            "Failed to read skill directory entry under {}: {err}",
                            self.root_dir.display()
                        )
                    })?;
                    let file_type = entry.file_type().map_err(|err| {
                        format!(
                            "Failed to stat skill directory entry {}: {err}",
                            entry.path().display()
                        )
                    })?;
                    if !file_type.is_dir() {
                        continue;
                    }

                    let dir_name = entry.file_name().to_string_lossy().to_string();
                    let metadata_path = entry.path().join(SKILL_METADATA_FILENAME);
                    let body_path = entry.path().join(SKILL_BODY_FILENAME);
                    if !metadata_path.is_file() {
                        return Err(format!(
                            "Skill directory {} is missing {}",
                            entry.path().display(),
                            SKILL_METADATA_FILENAME
                        ));
                    }
                    if !body_path.is_file() {
                        return Err(format!(
                            "Skill directory {} is missing {}",
                            entry.path().display(),
                            SKILL_BODY_FILENAME
                        ));
                    }

                    let skill = serde_json::from_str::<Skill>(
                        &std::fs::read_to_string(&metadata_path).map_err(|err| {
                            format!(
                                "Failed to read skill metadata {}: {err}",
                                metadata_path.display()
                            )
                        })?,
                    )
                    .map_err(|err| {
                        format!(
                            "Failed to parse skill metadata {}: {err}",
                            metadata_path.display()
                        )
                    })?;
                    validate_skill(&skill)?;
                    if skill.name != dir_name {
                        return Err(format!(
                            "Skill metadata {} name '{}' does not match directory '{}'",
                            metadata_path.display(),
                            skill.name,
                            dir_name
                        ));
                    }
                    if records.insert(skill.id.0.clone(), skill.clone()).is_some() {
                        return Err(format!(
                            "duplicate skill id {} discovered on disk",
                            skill.id
                        ));
                    }
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

fn validate_skill(skill: &Skill) -> Result<(), String> {
    if skill.id.0.trim().is_empty() {
        return Err("skill id must not be empty".to_string());
    }
    if skill.name.trim().is_empty() {
        return Err(format!("skill {} name must not be empty", skill.id));
    }
    if skill.name.contains(std::path::MAIN_SEPARATOR)
        || skill.name.contains('/')
        || skill.name.contains('\\')
    {
        return Err(format!(
            "skill {} name '{}' must be a single directory name",
            skill.id, skill.name
        ));
    }
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
