use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Instant, SystemTime};

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
    sessions: Arc<Mutex<HashMap<SessionId, Weak<SessionJournal>>>>,
}

#[derive(Debug)]
pub(crate) struct SessionJournal {
    store: TranscriptStore,
    session_id: SessionId,
    index: Mutex<Option<TranscriptIndex>>,
}

impl Drop for SessionJournal {
    fn drop(&mut self) {
        // A replacement may have opened after this owner's strong count reached zero.

        if let Ok(mut sessions) = self.store.sessions.lock()
            && sessions
                .get(&self.session_id)
                .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            sessions.remove(&self.session_id);
        }
    }
}

#[derive(Debug, Default)]
struct TranscriptIndex {
    stamp: Option<JournalStamp>,
    next_sequence: u64,
    event_ids: HashSet<String>,
    provider_identities: HashSet<ProviderEventIdentity>,
}

impl TranscriptIndex {
    fn include(&mut self, record: &TranscriptRecord) {
        self.next_sequence = self.next_sequence.max(record.sequence.saturating_add(1));
        self.event_ids.insert(record.event_id.clone());
        if let Some(identity) = &record.provider_identity {
            self.provider_identities.insert(identity.clone());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JournalStamp {
    bytes: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl JournalStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self, String> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            bytes: metadata.len(),
            modified: metadata.modified().map_err(|error| {
                format!("failed to inspect transcript modification time: {error}")
            })?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

impl TranscriptStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn actor_io_enabled(&self) -> bool {
        true
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

    fn append(&self, record: &TranscriptRecord) -> Result<JournalStamp, String> {
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
            .map_err(|error| format!("failed to sync {}: {error}", path.display()))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        JournalStamp::from_metadata(&metadata)
    }

    pub(crate) fn open_session(&self, session_id: &SessionId) -> Arc<SessionJournal> {
        let mut sessions = self
            .sessions
            .lock()
            .expect("transcript session registry poisoned");
        if let Some(session) = sessions.get(session_id).and_then(Weak::upgrade) {
            return session;
        }
        let session = Arc::new(SessionJournal {
            store: self.clone(),
            session_id: session_id.clone(),
            index: Mutex::new(None),
        });
        sessions.insert(session_id.clone(), Arc::downgrade(&session));
        session
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

impl SessionJournal {
    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn append_import_if_missing(
        &self,
        record: &TranscriptRecord,
    ) -> Result<bool, String> {
        self.append_records(vec![record.clone()], false)
            .map(|appended| appended != 0)
    }

    pub(crate) fn append_live_records(&self, records: Vec<TranscriptRecord>) -> Result<(), String> {
        self.append_records(records, true).map(|_| ())
    }

    fn append_records(
        &self,
        records: Vec<TranscriptRecord>,
        assign_sequence: bool,
    ) -> Result<usize, String> {
        let started = Instant::now();
        let session_id = &self.session_id;
        let mut cached = self
            .index
            .lock()
            .map_err(|error| format!("transcript index poisoned: {error}"))?;
        let result = (|| {
            let path = self.store.journal_path(session_id);
            let stamp = match std::fs::metadata(&path) {
                Ok(metadata) => Some(JournalStamp::from_metadata(&metadata)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(format!("failed to inspect {}: {error}", path.display())),
            };
            if cached.as_ref().is_none_or(|index| index.stamp != stamp) {
                let mut index = TranscriptIndex {
                    stamp,
                    ..Default::default()
                };
                let existing = self.store.load(session_id)?;
                tracing::debug!(%session_id, records = existing.len(), "loading transcript append index");
                for record in existing {
                    index.include(&record);
                }
                *cached = Some(index);
            }
            let index = cached
                .as_mut()
                .ok_or("transcript index was not initialized")?;
            let mut appended = 0;
            for mut record in records {
                if record.logical_session_id != *session_id {
                    return Err("transcript append batch contains multiple sessions".to_owned());
                }
                if index.event_ids.contains(&record.event_id)
                    || record
                        .provider_identity
                        .as_ref()
                        .is_some_and(|identity| index.provider_identities.contains(identity))
                {
                    continue;
                }
                if assign_sequence {
                    record.sequence = index.next_sequence;
                }
                index.stamp = Some(self.store.append(&record)?);
                index.include(&record);
                appended += 1;
            }
            Ok(appended)
        })();
        if result.is_err() {
            *cached = None;
        }
        tracing::debug!(%session_id, elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(), "completed transcript append batch");
        result
    }
}
