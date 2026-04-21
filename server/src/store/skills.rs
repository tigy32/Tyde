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

    pub fn load_body(&self, id: &SkillId) -> Result<String, String> {
        let skill = self
            .get(id)
            .ok_or_else(|| format!("cannot resolve missing skill {}", id))?;
        let path = self.root_dir.join(&skill.name).join(SKILL_BODY_FILENAME);
        std::fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read skill body {}: {err}", path.display()))
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
        let mut records = HashMap::new();
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

                    let dir_name = entry.file_name().to_string_lossy().to_string();
                    let Some(skill) = load_skill_from_dir(&entry.path(), &dir_name) else {
                        continue;
                    };
                    if records.insert(skill.id.0.clone(), skill.clone()).is_some() {
                        tracing::warn!(
                            skill_id = %skill.id,
                            path = %entry.path().display(),
                            "duplicate skill id discovered on disk; skipping duplicate"
                        );
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

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("tyde-skill-store-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap_or_else(|err| {
                panic!("failed to create test dir {}: {err}", path.display())
            });
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_skill_body(root: &std::path::Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|err| panic!("failed to create skill dir {}: {err}", dir.display()));
        std::fs::write(dir.join(SKILL_BODY_FILENAME), body)
            .unwrap_or_else(|err| panic!("failed to write skill body for {name}: {err}"));
    }

    #[test]
    fn list_accepts_skill_without_metadata() {
        let fixture = TestDir::new("metadata-optional");
        let index_path = fixture.path.join("skills.json");
        let root_dir = fixture.path.join("skills");
        write_skill_body(&root_dir, "lint", "# lint\n");

        let store = SkillStore::load(index_path, root_dir).expect("load skill store");
        let skills = store.list().expect("list skills");

        assert_eq!(
            skills,
            vec![Skill {
                id: SkillId("lint".to_string()),
                name: "lint".to_string(),
                title: None,
                description: None,
            }]
        );
    }

    #[test]
    fn list_skips_malformed_metadata_without_failing() {
        let fixture = TestDir::new("skip-bad-metadata");
        let index_path = fixture.path.join("skills.json");
        let root_dir = fixture.path.join("skills");

        write_skill_body(&root_dir, "good-skill", "# good\n");
        std::fs::write(
            root_dir.join("good-skill").join(SKILL_METADATA_FILENAME),
            serde_json::to_string_pretty(&Skill {
                id: SkillId("good".to_string()),
                name: "good-skill".to_string(),
                title: Some("Good".to_string()),
                description: Some("Works".to_string()),
            })
            .expect("serialize good metadata"),
        )
        .expect("write good metadata");

        write_skill_body(&root_dir, "bad-skill", "# bad\n");
        std::fs::write(
            root_dir.join("bad-skill").join(SKILL_METADATA_FILENAME),
            "{not-json",
        )
        .expect("write bad metadata");

        let store = SkillStore::load(index_path, root_dir).expect("load skill store");
        let skills = store.list().expect("list skills");

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, SkillId("good".to_string()));
        assert_eq!(skills[0].name, "good-skill");
    }

    #[test]
    fn load_rebuilds_invalid_index_from_disk() {
        let fixture = TestDir::new("rebuild-invalid-index");
        let index_path = fixture.path.join("skills.json");
        let root_dir = fixture.path.join("skills");
        write_skill_body(&root_dir, "ops", "# ops\n");
        std::fs::write(&index_path, "{ definitely-invalid-json").expect("write invalid index");

        let store = SkillStore::load(index_path.clone(), root_dir).expect("load skill store");
        let skill = store
            .get(&SkillId("ops".to_string()))
            .expect("expected rebuilt skill");

        assert_eq!(skill.id, SkillId("ops".to_string()));
        assert!(
            std::fs::read_to_string(index_path)
                .expect("read rebuilt index")
                .contains("\"ops\"")
        );
    }
}
