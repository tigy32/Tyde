use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
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

/// Why a journal read stopped before the end of the file.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum JournalDamage {
    #[default]
    None,
    /// Bytes that are not a finished record. No writer ever committed them, so
    /// discarding them destroys nothing.
    Torn,
    /// A finished record this build cannot deserialize. Never discarded: a
    /// build that does understand it must still be able to read it, and the
    /// bytes are the only copy.
    Unreadable,
}

/// A journal read, plus where its intact prefix ends.
#[derive(Default)]
struct LoadedJournal {
    records: Vec<TranscriptRecord>,
    intact_len: u64,
    file_len: u64,
    damage: JournalDamage,
}

/// What one append batch did, beyond the records it wrote.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TranscriptAppend {
    /// Records actually written. Zero means nothing was attempted or every
    /// record deduplicated — either way this batch did not prove the journal
    /// writable, and nothing should read recovery into it.
    pub appended: usize,
    /// Bytes of torn record discarded to keep later records reachable.
    pub discarded_bytes: u64,
    /// The journal holds a finished record this build cannot read, so the
    /// history before it is all any reader can serve.
    pub unreadable_record: bool,
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

    /// Append one record, or leave the journal exactly as it was.
    ///
    /// The record and its terminator go out in a single `write_all` so a short
    /// write is the only way to tear one, and a full disk is short-write
    /// territory: `write_all` reports `ENOSPC` *after* committing part of the
    /// record. Those orphan bytes used to poison every later read of the
    /// journal, so a failed write rewinds the file to the length it had before
    /// the attempt.
    fn append(&self, record: &TranscriptRecord) -> Result<JournalStamp, String> {
        self.ensure_root()?;
        let path = self.journal_path(&record.logical_session_id);
        let mut encoded = serde_json::to_vec(record)
            .map_err(|error| format!("failed to encode transcript record: {error}"))?;
        encoded.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        let committed = file
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
            .len();
        if let Err(error) = file.write_all(&encoded).and_then(|_| file.sync_data()) {
            let rollback = file.set_len(committed).and_then(|()| file.sync_data());
            return Err(match rollback {
                Ok(()) => format!("failed to append {}: {error}", path.display()),
                Err(rollback) => format!(
                    "failed to append {}: {error}; discarding the torn record also failed: {rollback}",
                    path.display()
                ),
            });
        }
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
        let mut journal = self.read_journal(session_id)?;
        journal.records.sort_by_key(|record| record.sequence);
        Ok(journal.records)
    }

    /// Read the journal for a writer, dropping a torn record from the file.
    ///
    /// Reading stops at the first torn record, so anything appended past one
    /// would be written where no reader can ever reach it. Truncating the
    /// damage away before appending is what keeps a journal that lost its tail
    /// usable instead of frozen.
    fn read_journal_for_append(
        &self,
        session_id: &SessionId,
    ) -> Result<(Vec<TranscriptRecord>, TranscriptAppend), String> {
        let journal = self.read_journal(session_id)?;
        let mut outcome = TranscriptAppend {
            unreadable_record: journal.damage == JournalDamage::Unreadable,
            ..TranscriptAppend::default()
        };
        // Only bytes no writer finished are safe to remove. A record this build
        // cannot deserialize is left exactly where it is, even though that
        // leaves everything appended after it unreachable to this build: the
        // journal is the only copy, and a build that understands the record
        // needs all of it.
        if journal.damage == JournalDamage::Torn && journal.intact_len < journal.file_len {
            let path = self.journal_path(session_id);
            let file = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
            file.set_len(journal.intact_len)
                .and_then(|()| file.sync_data())
                .map_err(|error| {
                    format!("failed to discard torn tail of {}: {error}", path.display())
                })?;
            outcome.discarded_bytes = journal.file_len - journal.intact_len;
            tracing::warn!(
                path = %path.display(),
                discarded_bytes = outcome.discarded_bytes,
                retained_records = journal.records.len(),
                "discarded a torn transcript record so later records stay readable"
            );
        }
        Ok((journal.records, outcome))
    }

    /// Parse the journal, stopping at the first record the writer did not
    /// finish.
    ///
    /// A journal is append-only, so an unreadable line is a record torn by a
    /// full disk or a crash mid-write rather than a schema problem — and
    /// everything before it is intact. Failing the whole file over that tail
    /// turned one bad write into a session that could never be read or resumed
    /// again, so the intact prefix is kept and the damage is reported loudly.
    ///
    /// A record that parses but disagrees with the file it sits in is a
    /// different animal: that is a logic bug, not a torn write, and it still
    /// fails the read.
    fn read_journal(&self, session_id: &SessionId) -> Result<LoadedJournal, String> {
        let path = self.journal_path(session_id);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedJournal::default());
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let mut records = Vec::new();
        let mut event_ids = HashSet::new();
        let mut offset = 0;
        let mut intact_len = 0;
        let mut line_number = 0;
        let mut damage = JournalDamage::None;
        while offset < bytes.len() {
            line_number += 1;
            let Some(newline) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
                damage = JournalDamage::Torn;
                tracing::error!(
                    path = %path.display(),
                    line = line_number,
                    retained_records = records.len(),
                    trailing_bytes = bytes.len() - offset,
                    "transcript record was never terminated; keeping the records before it"
                );
                break;
            };
            let line = &bytes[offset..offset + newline];
            let next = offset + newline + 1;
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                offset = next;
                intact_len = next;
                continue;
            }
            let record: TranscriptRecord = match serde_json::from_slice(line) {
                Ok(record) => record,
                Err(error) => {
                    // A record that is valid JSON but the wrong shape was
                    // finished by its writer — most likely a build that knows
                    // an event variant this one does not. Reading still stops,
                    // because guessing past it would mis-sequence everything
                    // after, but the bytes stay: this build not understanding
                    // a record is no licence to delete it.
                    damage = match error.classify() {
                        serde_json::error::Category::Data => JournalDamage::Unreadable,
                        _ => JournalDamage::Torn,
                    };
                    tracing::error!(
                        path = %path.display(),
                        line = line_number,
                        retained_records = records.len(),
                        trailing_bytes = bytes.len() - offset,
                        recoverable = damage == JournalDamage::Torn,
                        %error,
                        "transcript record could not be read; keeping the records before it"
                    );
                    break;
                }
            };
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
            offset = next;
            intact_len = next;
        }
        Ok(LoadedJournal {
            records,
            intact_len: intact_len as u64,
            file_len: bytes.len() as u64,
            damage,
        })
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
            .map(|outcome| outcome.appended != 0)
    }

    pub(crate) fn append_live_records(
        &self,
        records: Vec<TranscriptRecord>,
    ) -> Result<TranscriptAppend, String> {
        self.append_records(records, true)
    }

    fn append_records(
        &self,
        records: Vec<TranscriptRecord>,
        assign_sequence: bool,
    ) -> Result<TranscriptAppend, String> {
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
            let mut outcome = TranscriptAppend::default();
            if cached.as_ref().is_none_or(|index| index.stamp != stamp) {
                let mut index = TranscriptIndex {
                    stamp,
                    ..Default::default()
                };
                let (existing, read) = self.store.read_journal_for_append(session_id)?;
                outcome = read;
                tracing::debug!(%session_id, records = existing.len(), "loading transcript append index");
                for record in existing {
                    index.include(&record);
                }
                *cached = Some(index);
            }
            let index = cached
                .as_mut()
                .ok_or("transcript index was not initialized")?;
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
                outcome.appended += 1;
            }
            Ok(outcome)
        })();
        if result.is_err() {
            *cached = None;
        }
        tracing::debug!(%session_id, elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(), "completed transcript append batch");

        result
    }
}
