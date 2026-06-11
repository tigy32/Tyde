use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{
    AgentId, ProjectGitDiffPayload, ProjectId, Review, ReviewAiReviewerState, ReviewComment,
    ReviewDiffSelection, ReviewId, ReviewStatus, ReviewSuggestedComment, SessionId,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    records: HashMap<ReviewId, StoredReview>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredReview {
    Current(Review),
    Legacy(LegacyReview),
}

impl StoredReview {
    fn into_review(self) -> (Review, bool) {
        match self {
            StoredReview::Current(review) => (review, false),
            StoredReview::Legacy(review) => (review.into_review(), true),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyReview {
    id: ReviewId,
    project_id: ProjectId,
    origin: LegacyReviewOrigin,
    selection: LegacyReviewDiffSelection,
    status: ReviewStatus,
    #[serde(default)]
    diffs: Vec<ProjectGitDiffPayload>,
    #[serde(default)]
    comments: Vec<ReviewComment>,
    #[serde(default)]
    suggestions: Vec<ReviewSuggestedComment>,
    ai_reviewer: ReviewAiReviewerState,
    created_at_ms: u64,
    updated_at_ms: u64,
}

impl LegacyReview {
    fn into_review(self) -> Review {
        let (origin_agent_id, origin_session_id) = self.origin.into_parts(&self.id);
        Review {
            id: self.id,
            project_id: self.project_id,
            origin_agent_id,
            origin_session_id,
            selection: self.selection.into_selection(),
            status: self.status,
            diffs: self.diffs,
            comments: self.comments,
            suggestions: self.suggestions,
            ai_reviewer: self.ai_reviewer,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LegacyReviewDiffSelection {
    AllUncommitted,
    AllUnpushed,
    Root {
        root: protocol::ProjectRootPath,
        scope: protocol::ProjectDiffScope,
        path: Option<String>,
    },
}

impl LegacyReviewDiffSelection {
    fn into_selection(self) -> ReviewDiffSelection {
        match self {
            LegacyReviewDiffSelection::AllUncommitted | LegacyReviewDiffSelection::AllUnpushed => {
                ReviewDiffSelection::AllUncommitted
            }
            LegacyReviewDiffSelection::Root { root, scope, path } => {
                ReviewDiffSelection::Root { root, scope, path }
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LegacyReviewOrigin {
    Agent {
        agent_id: AgentId,
        session_id: SessionId,
    },
    Project,
}

impl LegacyReviewOrigin {
    fn into_parts(self, review_id: &ReviewId) -> (AgentId, SessionId) {
        match self {
            LegacyReviewOrigin::Agent {
                agent_id,
                session_id,
            } => (agent_id, session_id),
            LegacyReviewOrigin::Project => {
                let id = format!("legacy-project-origin:{}", review_id.0);
                (AgentId(id.clone()), SessionId(id))
            }
        }
    }
}

#[derive(Debug)]
struct ReadStore {
    records: HashMap<ReviewId, Review>,
    migrated: bool,
}

#[derive(Debug, Clone)]
pub struct ReviewStore {
    path: PathBuf,
}

impl ReviewStore {
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let loaded = Self::read_store_from_disk(&path)?;
        let store = Self { path };
        if loaded.migrated
            || loaded
                .records
                .values()
                .any(|review| !review.diffs.is_empty())
        {
            store.save(&loaded.records)?;
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

        Ok(crate::paths::home_dir()?.join(".tyde").join("reviews.json"))
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

    /// Deletes every persisted review referencing `project_id` and returns
    /// the ids of the removed reviews. Used when a project or workbench is
    /// deleted so the store does not accumulate orphaned records.
    pub fn delete_for_project(&self, project_id: &ProjectId) -> Result<Vec<ReviewId>, String> {
        let mut records = Self::read_from_disk(&self.path)?;
        let removed = records
            .iter()
            .filter(|(_, review)| &review.project_id == project_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if removed.is_empty() {
            return Ok(removed);
        }
        for id in &removed {
            records.remove(id);
        }
        self.save(&records)?;
        Ok(removed)
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
        Ok(Self::read_store_from_disk(path)?.records)
    }

    fn read_store_from_disk(path: &Path) -> Result<ReadStore, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<StoreFile>(&contents)
                .map(|store| {
                    let mut migrated = false;
                    let records = store
                        .records
                        .into_iter()
                        .map(|(id, record)| {
                            let (review, record_migrated) = record.into_review();
                            migrated |= record_migrated;
                            (id, review)
                        })
                        .collect();
                    ReadStore { records, migrated }
                })
                .map_err(|err| format!("Failed to parse review store {}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ReadStore {
                records: HashMap::new(),
                migrated: false,
            }),
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
            records: compact_records
                .into_iter()
                .map(|(id, review)| (id, StoredReview::Current(review)))
                .collect(),
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
    use serde_json::Value;

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
            records: HashMap::from([(review.id.clone(), StoredReview::Current(review))]),
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

    #[test]
    fn review_store_migrates_legacy_agent_origin() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("reviews.json");
        std::fs::write(
            &path,
            r#"{
  "records": {
    "review-1": {
      "id": "review-1",
      "project_id": "project-1",
      "origin": {
        "kind": "agent",
        "agent_id": "agent-1",
        "session_id": "session-1"
      },
      "selection": {
        "kind": "all_uncommitted"
      },
      "status": {
        "state": "draft"
      },
      "diffs": [],
      "comments": [],
      "suggestions": [],
      "ai_reviewer": {
        "status": "idle",
        "agent_id": null,
        "error": null
      },
      "created_at_ms": 1,
      "updated_at_ms": 2
    }
  }
}"#,
        )
        .expect("write legacy agent-origin store");

        let store = ReviewStore::load(path.clone()).expect("load store");
        let review = store
            .get(&ReviewId("review-1".to_owned()))
            .expect("read migrated review")
            .expect("review");

        assert_eq!(review.origin_agent_id, AgentId("agent-1".to_owned()));
        assert_eq!(review.origin_session_id, SessionId("session-1".to_owned()));

        let contents = std::fs::read_to_string(path).expect("read migrated store");
        let value: Value = serde_json::from_str(&contents).expect("parse migrated store");
        let record = &value["records"]["review-1"];
        assert!(record.get("origin").is_none());
        assert_eq!(record["origin_agent_id"], "agent-1");
        assert_eq!(record["origin_session_id"], "session-1");
    }

    #[test]
    fn review_store_migrates_legacy_project_origin() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("reviews.json");
        std::fs::write(
            &path,
            r#"{
  "records": {
    "review-1": {
      "id": "review-1",
      "project_id": "project-1",
      "origin": {
        "kind": "project"
      },
      "selection": {
        "kind": "all_unpushed"
      },
      "status": {
        "state": "cancelled",
        "cancelled_at_ms": 3
      },
      "diffs": [],
      "comments": [],
      "suggestions": [],
      "ai_reviewer": {
        "status": "idle",
        "agent_id": null,
        "error": null
      },
      "created_at_ms": 1,
      "updated_at_ms": 3
    }
  }
}"#,
        )
        .expect("write legacy project-origin store");

        let store = ReviewStore::load(path.clone()).expect("load store");
        let review = store
            .get(&ReviewId("review-1".to_owned()))
            .expect("read migrated review")
            .expect("review");

        let expected_origin = "legacy-project-origin:review-1".to_owned();
        assert_eq!(review.origin_agent_id, AgentId(expected_origin.clone()));
        assert_eq!(review.origin_session_id, SessionId(expected_origin.clone()));
        assert!(matches!(
            review.selection,
            ReviewDiffSelection::AllUncommitted
        ));

        let contents = std::fs::read_to_string(path).expect("read migrated store");
        let value: Value = serde_json::from_str(&contents).expect("parse migrated store");
        let record = &value["records"]["review-1"];
        assert!(record.get("origin").is_none());
        assert_eq!(record["origin_agent_id"], expected_origin);
        assert_eq!(record["origin_session_id"], expected_origin);
    }
}
