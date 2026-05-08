use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{Review, ReviewId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<ReviewId, Review>,
}

#[derive(Debug, Clone)]
pub struct ReviewStore {
    path: PathBuf,
}

impl ReviewStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let records = Self::read_from_disk(&path)?;
        let store = Self { path };
        if records.values().any(|review| !review.diffs.is_empty()) {
            store.save(&records)?;
        }
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_REVIEW_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("reviews.json"))
    }

    pub fn list(&self) -> Result<Vec<Review>, String> {
        let records = Self::read_from_disk(&self.path)?;
        let mut out: Vec<_> = records.into_values().collect();
        out.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        Ok(out)
    }

    pub fn get(&self, id: &ReviewId) -> Result<Option<Review>, String> {
        Ok(Self::read_from_disk(&self.path)?.get(id).cloned())
    }

    pub fn upsert(&self, review: Review) -> Result<(), String> {
        self.read_modify_write(|records| {
            records.insert(review.id.clone(), review);
        })
    }

    fn read_modify_write<F>(&self, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut HashMap<ReviewId, Review>),
    {
        let mut records = Self::read_from_disk(&self.path)?;
        update(&mut records);
        self.save(&records)
    }

    fn read_from_disk(path: &Path) -> Result<HashMap<ReviewId, Review>, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<StoreFile>(&contents)
                .map(|store| store.records)
                .map_err(|err| format!("Failed to parse review store {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(format!(
                "Failed to read review store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(&self, records: &HashMap<ReviewId, Review>) -> Result<(), String> {
        let mut compact_records = records.clone();
        for review in compact_records.values_mut() {
            review.diffs.clear();
        }
        let json = serde_json::to_string_pretty(&StoreFile {
            records: compact_records,
        })
        .map_err(|err| format!("Failed to serialize review store: {err}"))?;

        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("Review store path has no parent: {}", self.path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create review store directory: {err}"))?;

        let tmp_path = self.path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp review store file: {err}"))?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp review store file: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("Failed to sync temp review store file: {err}"))?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            format!(
                "Failed to atomically replace review store {}: {err}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use protocol::{
        AgentId, DiffContextMode, ProjectDiffScope, ProjectGitDiffPayload, ProjectId,
        ProjectRootPath, ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewDiffSelection,
        ReviewStatus, SessionId,
    };

    use super::*;

    fn sample_review(id: &str, status: ReviewStatus) -> Review {
        Review {
            id: ReviewId(id.to_owned()),
            project_id: ProjectId("project-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("session-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/repo".to_owned()),
                scope: ProjectDiffScope::Uncommitted,
                path: None,
                context_mode: DiffContextMode::FullFile,
                files: Vec::new(),
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn review_store_round_trips_records() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("reviews.json");
        let store = ReviewStore::load(path.clone()).expect("load store");

        let draft = sample_review("review-1", ReviewStatus::Draft);
        let consumed = sample_review(
            "review-2",
            ReviewStatus::Consumed {
                submitted_at_ms: 2,
                consumed_at_ms: 3,
                target_agent_id: AgentId("agent-2".to_owned()),
            },
        );
        store.upsert(draft.clone()).expect("persist draft");
        store.upsert(consumed.clone()).expect("persist consumed");

        let reloaded = ReviewStore::load(path).expect("reload store");
        let mut persisted_draft = draft;
        persisted_draft.diffs.clear();
        let mut persisted_consumed = consumed;
        persisted_consumed.diffs.clear();
        assert_eq!(
            reloaded.get(&persisted_draft.id).expect("read draft"),
            Some(persisted_draft)
        );
        assert_eq!(
            reloaded.get(&persisted_consumed.id).expect("read consumed"),
            Some(persisted_consumed)
        );
    }

    #[test]
    fn review_store_compacts_legacy_diff_snapshots_on_load() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("reviews.json");
        let review = sample_review("review-1", ReviewStatus::Draft);
        let json = serde_json::to_string_pretty(&StoreFile {
            records: HashMap::from([(review.id.clone(), review)]),
        })
        .expect("serialize legacy store");
        std::fs::write(&path, json).expect("write legacy store");

        let store = ReviewStore::load(path).expect("load store");
        let stored = store
            .get(&ReviewId("review-1".to_owned()))
            .expect("read compacted review")
            .expect("review");
        assert!(stored.diffs.is_empty());
    }
}
