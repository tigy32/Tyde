use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::session::{SessionStore, SessionStoreHandle};

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
            file_snapshots: Vec::new(),
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

#[derive(Debug, Clone)]
pub struct ReviewStore {
    sessions: Arc<SessionStoreHandle>,
}

impl ReviewStore {
    pub(crate) fn new(sessions: Arc<SessionStoreHandle>) -> Self {
        Self { sessions }
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

    pub(crate) async fn list_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<Review>, String> {
        let project_id = project_id.clone();
        self.sessions
            .read(move |store| store.reviews_for_project(&project_id))
            .await
    }

    pub(crate) async fn upsert(&self, mut review: Review) -> Result<(), String> {
        review.diffs.clear();
        self.sessions
            .call(move |store| store.upsert_review(review))
            .await
    }

    pub(crate) async fn delete_for_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<ReviewId>, String> {
        let project_id = project_id.clone();
        self.sessions
            .call(move |store| store.delete_reviews_for_project(&project_id))
            .await
    }
}

impl SessionStore {
    pub(crate) fn initialize_reviews(
        &self,
        legacy_path: &Path,
    ) -> Result<Vec<(ReviewId, ProjectId)>, String> {
        let mut connection = self
            .writer
            .lock()
            .map_err(|_| "review database writer poisoned")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin review migration: {error}"))?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS reviews (
                id TEXT PRIMARY KEY NOT NULL,
                project_id TEXT NOT NULL,
                record TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS reviews_by_project ON reviews(project_id);
             CREATE TABLE IF NOT EXISTS review_migrations (name TEXT PRIMARY KEY NOT NULL);",
            )
            .map_err(|error| format!("initialize review schema: {error}"))?;
        let imported = transaction
            .query_row(
                "SELECT name FROM review_migrations WHERE name = 'reviews_json'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("read review migration: {error}"))?
            .is_some();
        if !imported {
            match std::fs::read_to_string(legacy_path) {
                Ok(contents) => {
                    let legacy: StoreFile = serde_json::from_str(&contents).map_err(|error| {
                        format!(
                            "parse legacy review store {}: {error}",
                            legacy_path.display()
                        )
                    })?;
                    for (id, stored) in legacy.records {
                        let (mut review, _) = stored.into_review();
                        if review.id != id {
                            return Err("legacy review key does not match its record id".to_owned());
                        }
                        review.diffs.clear();
                        let record = serde_json::to_string(&review)
                            .map_err(|error| format!("encode migrated review: {error}"))?;
                        transaction
                            .execute(
                                "INSERT INTO reviews(id, project_id, record) VALUES (?1, ?2, ?3)
                             ON CONFLICT(id) DO NOTHING",
                                params![review.id.0, review.project_id.0, record],
                            )
                            .map_err(|error| format!("import legacy review: {error}"))?;
                    }
                    transaction
                        .execute(
                            "INSERT INTO review_migrations(name) VALUES ('reviews_json')",
                            [],
                        )
                        .map_err(|error| format!("record review migration: {error}"))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "read legacy review store {}: {error}",
                        legacy_path.display()
                    ));
                }
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("commit review migration: {error}"))?;
        let mut statement = connection
            .prepare("SELECT id, project_id FROM reviews")
            .map_err(|error| format!("read review ownership: {error}"))?;
        statement
            .query_map([], |row| {
                Ok((ReviewId(row.get(0)?), ProjectId(row.get(1)?)))
            })
            .map_err(|error| format!("read review ownership: {error}"))?
            .map(|row| row.map_err(|error| format!("decode review ownership: {error}")))
            .collect()
    }

    fn reviews_for_project(&self, project_id: &ProjectId) -> Result<Vec<Review>, String> {
        let connection = self.reader()?;
        let mut statement = connection
            .prepare("SELECT record FROM reviews WHERE project_id = ?1")
            .map_err(|error| format!("query project reviews: {error}"))?;
        let rows = statement
            .query_map([&project_id.0], |row| row.get::<_, String>(0))
            .map_err(|error| format!("read project reviews: {error}"))?;
        rows.map(|row| {
            let record = row.map_err(|error| format!("read review record: {error}"))?;
            let review: Review = serde_json::from_str(&record)
                .map_err(|error| format!("decode review record: {error}"))?;
            if &review.project_id != project_id {
                return Err("review database ownership does not match its record".to_owned());
            }
            Ok(review)
        })
        .collect()
    }

    fn upsert_review(&self, review: Review) -> Result<(), String> {
        let record =
            serde_json::to_string(&review).map_err(|error| format!("encode review: {error}"))?;
        let connection = self
            .writer
            .lock()
            .map_err(|_| "review database writer poisoned")?;
        let changed = connection
            .execute(
                "INSERT INTO reviews(id, project_id, record) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET record = excluded.record
             WHERE reviews.project_id = excluded.project_id",
                params![review.id.0, review.project_id.0, record],
            )
            .map_err(|error| format!("persist review: {error}"))?;
        if changed != 1 {
            return Err("cannot change a review's project owner".to_owned());
        }
        Ok(())
    }

    fn delete_reviews_for_project(&self, project_id: &ProjectId) -> Result<Vec<ReviewId>, String> {
        let mut connection = self
            .writer
            .lock()
            .map_err(|_| "review database writer poisoned")?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin project review deletion: {error}"))?;
        let removed = {
            let mut statement = transaction
                .prepare("SELECT id FROM reviews WHERE project_id = ?1")
                .map_err(|error| format!("query deleted reviews: {error}"))?;
            statement
                .query_map([&project_id.0], |row| row.get::<_, String>(0))
                .map_err(|error| format!("read deleted reviews: {error}"))?
                .map(|row| {
                    row.map(ReviewId)
                        .map_err(|error| format!("decode deleted review id: {error}"))
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction
            .execute("DELETE FROM reviews WHERE project_id = ?1", [&project_id.0])
            .map_err(|error| format!("delete project reviews: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("commit project review deletion: {error}"))?;
        Ok(removed)
    }
}
