use std::collections::VecDeque;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::wire::{Position, RawRecord, StreamRef};

const FRAME_LIMIT: usize = 32 * 1024 * 1024;

pub(super) enum Entry {
    Record(RawRecord),
    Omitted {
        stream: StreamRef,
        position: Position,
    },
}

pub(super) struct NativeLog {
    reader: BufReader<tokio::fs::File>,
    partial: Vec<u8>,
    pending: VecDeque<Entry>,
    inherited_fork: bool,
}

#[derive(Deserialize)]
#[serde(tag = "retained_frame")]
enum Frame {
    #[serde(rename = "session_permission_transaction")]
    PermissionTransaction {
        frame_schema_version: u32,
        children: Vec<Child>,
    },
}

#[derive(Deserialize)]
struct Child {
    child_index: usize,
    record_json: String,
}

#[derive(Deserialize)]
#[serde(tag = "retained_marker")]
enum Marker {
    #[serde(rename = "omitted_live_only")]
    OmittedLiveOnly {
        schema_version: u32,
        stream: StreamRef,
        position: Position,
        omitted_record: Omission,
    },
}

#[derive(Deserialize)]
struct Omission {
    record_type: String,
    durability: String,
    payload_type: String,
    payload_schema_version: u32,
    payload_kind: String,
    omission_class: Option<String>,
}

impl NativeLog {
    pub async fn open(path: &str) -> Result<(Self, StreamRef, u64), String> {
        if !std::path::Path::new(path).is_absolute() {
            return Err("Muse did not expose an absolute durable session-log path".to_owned());
        }
        let file = tokio::fs::File::open(path).await.map_err(|error| {
            format!("Cannot open the native Muse session log: {}", error.kind())
        })?;
        if !file
            .metadata()
            .await
            .map_err(|error| {
                format!(
                    "Cannot inspect the native Muse session log: {}",
                    error.kind()
                )
            })?
            .is_file()
        {
            return Err("Muse session-log path is not a regular file".to_owned());
        }
        let mut log = Self {
            reader: BufReader::new(file),
            partial: Vec::new(),
            pending: VecDeque::new(),
            inherited_fork: false,
        };
        let first = log
            .next()
            .await?
            .ok_or("Muse durable session log was empty")?;
        let stream = match &first {
            Entry::Record(record) if record.sequence == 1 && record.stream.kind == "session" => {
                record.stream.clone()
            }
            _ => {
                return Err(
                    "Muse session log did not start at its authoritative session origin".to_owned(),
                );
            }
        };
        let mut fork_context_through = 0;
        if let Entry::Record(record) = &first
            && record.payload_type == "session.fork.created"
        {
            validate_fork_record(record, &stream)?;
            log.inherited_fork = true;
            let mut inherited = VecDeque::from([first]);
            let prefix = log
                .next()
                .await?
                .ok_or("Muse fork omitted its model-context checkpoint")?;
            match &prefix {
                Entry::Record(record)
                    if record.sequence == 2
                        && record.payload_type == "session.fork.model_prefix" =>
                {
                    validate_fork_record(record, &stream)?
                }
                _ => {
                    return Err(
                        "Muse fork did not start with its native context checkpoint".to_owned()
                    );
                }
            }
            inherited.push_back(prefix);
            let mut sequence = 2;
            loop {
                let entry = log
                    .next()
                    .await?
                    .ok_or("Muse fork omitted its native context boundary")?;
                let (entry_stream, position) = match &entry {
                    Entry::Record(record) => (
                        &record.stream,
                        Position {
                            id: record.id.clone(),
                            sequence: record.sequence,
                        },
                    ),
                    Entry::Omitted { stream, position } => (stream, position.clone()),
                };
                if entry_stream.id != stream.id
                    || entry_stream.kind != stream.kind
                    || position.sequence != sequence + 1
                {
                    return Err(
                        "Muse inherited fork positions were not contiguous in their owner stream"
                            .to_owned(),
                    );
                }
                sequence = position.sequence;
                if let Entry::Record(record) = &entry {
                    if matches!(
                        record.payload_type.as_str(),
                        "runtime.session.permission_format_declared"
                            | "runtime.reasoning_effort_reconfigure"
                    ) && record.payload_schema_version == 1
                    {
                        fork_context_through = sequence - 1;
                        log.inherited_fork = false;
                        inherited.push_back(entry);
                        break;
                    }
                    if record.payload_type != "session.fork.turn" {
                        return Err(
                            "Muse fork context contained an unsupported native record".to_owned()
                        );
                    }
                    validate_fork_record(record, &stream)?;
                }
                inherited.push_back(entry);
            }
            inherited.append(&mut log.pending);
            log.pending = inherited;
        } else {
            log.pending.push_front(first);
        }
        Ok((log, stream, fork_context_through))
    }

    pub async fn next(&mut self) -> Result<Option<Entry>, String> {
        loop {
            if let Some(entry) = self.pending.pop_front() {
                return Ok(Some(entry));
            }
            loop {
                let available = self.reader.fill_buf().await.map_err(|error| {
                    format!("Cannot read the native Muse session log: {}", error.kind())
                })?;
                if available.is_empty() {
                    return Ok(None);
                }
                let count = available
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(available.len(), |index| index + 1);
                let complete = available[count - 1] == b'\n';
                if self.partial.len() + count > FRAME_LIMIT {
                    return Err(
                        "Muse session storage exceeded the native frame admission bound".to_owned(),
                    );
                }
                self.partial.extend_from_slice(&available[..count]);
                self.reader.consume(count);
                if complete {
                    break;
                }
            }
            let value: Value = serde_json::from_slice(&self.partial)
                .map_err(|_| "Muse session storage contained invalid JSON".to_owned())?;
            self.partial.clear();
            if value.get("retained_frame").is_some() {
                let Frame::PermissionTransaction {
                    frame_schema_version,
                    children,
                } = decode(value)?;
                if frame_schema_version != 1 || children.is_empty() {
                    return Err("Muse changed its retained-frame storage schema".to_owned());
                }
                for (index, child) in children.into_iter().enumerate() {
                    if child.child_index != index {
                        return Err(
                            "Muse retained transaction child positions were not contiguous"
                                .to_owned(),
                        );
                    }
                    let record = serde_json::from_str(&child.record_json).map_err(|_| {
                        "Muse retained transaction contained an invalid native envelope".to_owned()
                    })?;
                    self.pending.push_back(Entry::Record(record));
                }
            } else if value.get("retained_marker").is_some() {
                let Marker::OmittedLiveOnly {
                    schema_version,
                    stream,
                    position,
                    omitted_record: omitted,
                } = decode(value)?;
                // Forks replace parent durable runs with a validated model checkpoint.
                // That omission is valid only inside the native inherited-context boundary.
                let transient = omitted.record_type == "status"
                    && omitted.durability == "ephemeral"
                    && omitted.payload_kind == "task"
                    && omitted.omission_class.as_deref() == Some("task_tool_delta_v1");
                let inherited = self.inherited_fork
                    && omitted.record_type == "event"
                    && omitted.durability == "durable"
                    && omitted.payload_kind == "run"
                    && omitted.omission_class.is_none();
                if schema_version != 1
                    || omitted.payload_type != "runtime.session"
                    || omitted.payload_schema_version != 1
                    || !(transient || inherited)
                {
                    return Err(
                        "Muse retained storage omitted a required or unsupported native fact"
                            .to_owned(),
                    );
                }
                self.pending.push_back(Entry::Omitted { stream, position });
            } else {
                self.pending.push_back(Entry::Record(decode(value)?));
            }
        }
    }
}

fn validate_fork_record(record: &RawRecord, stream: &StreamRef) -> Result<(), String> {
    if record.schema_version != 1
        || record.payload_schema_version
            != match record.payload_type.as_str() {
                "session.fork.created" => 5,
                "session.fork.model_prefix" | "session.fork.turn" => 2,
                _ => return Err("Muse fork contained an unsupported checkpoint type".to_owned()),
            }
        || record.payload.get("schema_version").and_then(Value::as_u64) != Some(1)
        || record
            .payload
            .get("fork_session_id")
            .and_then(Value::as_str)
            != Some(stream.id.as_str())
        || record.stream.id != stream.id
        || record.stream.kind != stream.kind
    {
        return Err("Muse fork checkpoint omitted its validated native schema or owner".to_owned());
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|_| "Muse session storage did not match its pinned native schema".to_owned())
}
