use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use protocol::{ChatEvent, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranscriptVisibility {
    Visible,
    TimelineMarker,
    InternalCompactionSeed,
    ProviderMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ProviderEventIdentity {
    pub backend: String,
    pub provider_session_id: String,
    pub event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TranscriptRecord {
    pub logical_session_id: SessionId,
    pub sequence: u64,
    pub event_id: String,
    pub visibility: TranscriptVisibility,
    #[serde(default)]
    pub provider_identity: Option<ProviderEventIdentity>,
    pub event: ChatEvent,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptStore {
    root: PathBuf,
    #[cfg(test)]
    actor_io_enabled: bool,
}

impl TranscriptStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            #[cfg(test)]
            actor_io_enabled: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_actor_io_enabled(mut self, enabled: bool) -> Self {
        self.actor_io_enabled = enabled;
        self
    }

    pub(crate) fn actor_io_enabled(&self) -> bool {
        #[cfg(not(test))]
        {
            true
        }
        #[cfg(test)]
        {
            self.actor_io_enabled || std::env::var_os("TYDE_TRANSCRIPT_STORE_DIR").is_some()
        }
    }

    pub(crate) fn default_root() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_TRANSCRIPT_STORE_DIR") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }
        Ok(crate::paths::home_dir()?.join(".tyde").join("transcripts"))
    }

    pub(crate) fn is_authoritative(&self, session_id: &SessionId) -> bool {
        self.authoritative_path(session_id).is_file()
    }

    pub(crate) fn mark_authoritative(&self, session_id: &SessionId) -> Result<(), String> {
        self.ensure_root()?;
        let path = self.authoritative_path(session_id);
        let mut file = File::create(&path)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        file.write_all(b"v1\n")
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        file.sync_data()
            .map_err(|error| format!("failed to sync {}: {error}", path.display()))
    }

    pub(crate) fn append(&self, record: &TranscriptRecord) -> Result<(), String> {
        self.ensure_root()?;
        let path = self.journal_path(&record.logical_session_id);
        let encoded = serde_json::to_vec(record)
            .map_err(|error| format!("failed to encode transcript record: {error}"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        file.write_all(&encoded)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|error| format!("failed to append {}: {error}", path.display()))?;
        file.sync_data()
            .map_err(|error| format!("failed to sync {}: {error}", path.display()))
    }

    pub(crate) fn append_import_if_missing(
        &self,
        record: &TranscriptRecord,
    ) -> Result<bool, String> {
        let records = self.load(&record.logical_session_id)?;
        let duplicate = records.iter().any(|existing| {
            existing.event_id == record.event_id
                || (record.provider_identity.is_some()
                    && existing.provider_identity == record.provider_identity)
        });
        if duplicate {
            return Ok(false);
        }
        self.append(record)?;
        Ok(true)
    }

    pub(crate) fn load(&self, session_id: &SessionId) -> Result<Vec<TranscriptRecord>, String> {
        let path = self.journal_path(session_id);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("failed to open {}: {error}", path.display())),
        };
        let mut records = Vec::new();
        let mut event_ids = HashSet::new();
        for (line_index, line) in BufReader::new(file).lines().enumerate() {
            let line =
                line.map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let record: TranscriptRecord = serde_json::from_str(&line).map_err(|error| {
                format!(
                    "failed to parse {} line {}: {error}",
                    path.display(),
                    line_index + 1
                )
            })?;
            if record.logical_session_id != *session_id {
                return Err(format!(
                    "transcript {} contains record for {}",
                    path.display(),
                    record.logical_session_id
                ));
            }
            if !event_ids.insert(record.event_id.clone()) {
                return Err(format!(
                    "transcript {} contains duplicate event id {}",
                    path.display(),
                    record.event_id
                ));
            }
            records.push(record);
        }
        records.sort_by_key(|record| record.sequence);
        Ok(records)
    }

    #[cfg(test)]
    fn window_before(
        &self,
        session_id: &SessionId,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<(Vec<TranscriptRecord>, bool), String> {
        let records = self.load(session_id)?;
        let eligible = records
            .into_iter()
            .filter(|record| before_sequence.is_none_or(|before| record.sequence < before))
            .collect::<Vec<_>>();
        let start = eligible.len().saturating_sub(limit);
        let has_more = start > 0;
        Ok((eligible.into_iter().skip(start).collect(), has_more))
    }

    fn ensure_root(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            format!(
                "failed to create transcript store {}: {error}",
                self.root.display()
            )
        })
    }

    fn journal_path(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(format!("{}.jsonl", safe_id(&session_id.0)))
    }

    fn authoritative_path(&self, session_id: &SessionId) -> PathBuf {
        self.root
            .join(format!("{}.authoritative", safe_id(&session_id.0)))
    }
}

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use protocol::{
        BackendKind, CompactionMethod, CompactionMetrics, CompactionMutation,
        CompactionObservationId, CompactionTrigger, ContextCompactionTimelineEvent,
        ContextCompactionTimelineStatus,
    };
    use uuid::Uuid;

    use super::*;

    fn store() -> TranscriptStore {
        TranscriptStore::new(
            std::env::temp_dir().join(format!("tyde-transcript-test-{}", Uuid::new_v4())),
        )
    }

    fn marker(session_id: &SessionId, sequence: u64) -> TranscriptRecord {
        TranscriptRecord {
            logical_session_id: session_id.clone(),
            sequence,
            event_id: format!("event-{sequence}"),
            visibility: TranscriptVisibility::TimelineMarker,
            provider_identity: None,
            event: ChatEvent::ContextCompaction(ContextCompactionTimelineEvent {
                marker_id: CompactionObservationId(format!("marker-{sequence}")),
                operation_id: None,
                trigger: CompactionTrigger::BackendAutomatic,
                method: CompactionMethod::BackendAutomatic,
                backend_kind: BackendKind::Claude,
                provider_session_id: None,
                status: ContextCompactionTimelineStatus::Completed,
                mutation: CompactionMutation::Completed,
                metrics: CompactionMetrics::default(),
                message: None,
                timestamp: sequence,
            }),
            timestamp_ms: sequence,
        }
    }

    #[test]
    fn append_load_and_window_preserve_marker_order() {
        let store = store();
        let session_id = SessionId("session".to_owned());
        store.append(&marker(&session_id, 1)).expect("append one");
        store.append(&marker(&session_id, 2)).expect("append two");
        let (window, has_more) = store
            .window_before(&session_id, None, 1)
            .expect("read window");
        assert!(has_more);
        assert_eq!(window[0].sequence, 2);
    }

    #[test]
    fn provider_import_is_idempotent() {
        let store = store();
        let session_id = SessionId("session".to_owned());
        let mut record = marker(&session_id, 1);
        record.provider_identity = Some(ProviderEventIdentity {
            backend: "claude".to_owned(),
            provider_session_id: "provider".to_owned(),
            event_id: "boundary".to_owned(),
        });
        assert!(
            store
                .append_import_if_missing(&record)
                .expect("first import")
        );
        let mut duplicate = record.clone();
        duplicate.event_id = "different-local-id".to_owned();
        assert!(
            !store
                .append_import_if_missing(&duplicate)
                .expect("duplicate import")
        );
    }

    #[test]
    fn authority_is_explicit() {
        let store = store();
        let session_id = SessionId("session".to_owned());
        assert!(!store.is_authoritative(&session_id));
        store
            .mark_authoritative(&session_id)
            .expect("mark authoritative");
        assert!(store.is_authoritative(&session_id));
    }

    #[test]
    fn safe_id_never_creates_subdirectories() {
        assert_eq!(safe_id("../session/name"), "___session_name");
        assert!(!Path::new(&safe_id("../session/name")).is_absolute());
    }
}
