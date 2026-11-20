//! Durable per-response and per-turn latency/throughput samples.
//!
//! Agents never touch the database: each actor folds its own live chat events
//! into a [`ResponseSampler`] and hands finished samples to one writer thread
//! with a non-blocking send. The writer owns `metrics.sqlite` and commits
//! whatever is queued in one transaction, so batches grow with load instead of
//! with a timer. See `dev-docs/38-response-metrics.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentStartPayload, ChatEvent, ChatMessage, MessageSender, SessionSettingValue,
    SessionSettingsValues, TokenUsage,
};
use rusqlite::{Connection, Transaction, params};
use tokio::time::Instant;

const QUEUE_CAPACITY: usize = 16_384;
const SCHEMA_VERSION: u32 = 1;
const TTFT_BUCKETS_MS: [u64; 8] = [250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000];
const TPS_BUCKETS: [f64; 7] = [10.0, 25.0, 50.0, 75.0, 100.0, 150.0, 250.0];

#[derive(Clone)]
pub(crate) struct ResponseMetrics {
    tx: SyncSender<WriterMessage>,
    dropped: Arc<AtomicU64>,
    path: Arc<PathBuf>,
}

enum WriterMessage {
    Response(Box<ResponseSample>),
    Turn(Box<TurnSample>),
    #[cfg(any(test, feature = "test-support"))]
    Flush(tokio::sync::oneshot::Sender<()>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Completed,
    Cancelled,
    Error,
    Interrupted,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone)]
struct Labels {
    backend: String,
    settings_key: Option<String>,
    settings_json: Option<String>,
    agent_id: String,
    session_id: Option<String>,
    project_id: Option<String>,
    custom_agent_id: Option<String>,
    origin: String,
    is_child: bool,
}

struct ResponseSample {
    labels: Labels,
    model: Option<String>,
    ended_at_ms: u64,
    streamed: bool,
    ttft_from_input_ms: Option<u64>,
    ttft_from_start_ms: Option<u64>,
    generation_ms: Option<u64>,
    total_ms: Option<u64>,
    usage: Option<TokenUsage>,
    outcome: Outcome,
}

struct TurnSample {
    labels: Labels,
    started_at_ms: u64,
    ended_at_ms: u64,
    total_ms: u64,
    model_ms: u64,
    responses: u32,
    usage_complete: bool,
    usage: TokenUsage,
    outcome: Outcome,
}

impl ResponseMetrics {
    pub(crate) fn spawn(path: PathBuf) -> Result<Self, String> {
        let connection = open(&path)?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_dropped = Arc::clone(&dropped);
        std::thread::Builder::new()
            .name("tyde-response-metrics".to_owned())
            .spawn(move || run_writer(connection, rx, writer_dropped))
            .map_err(|error| format!("start response metrics writer: {error}"))?;
        Ok(Self {
            tx,
            dropped,
            path: Arc::new(path),
        })
    }

    pub(crate) fn path_for_session_store(session_store: &Path) -> PathBuf {
        session_store.with_file_name("metrics.sqlite")
    }

    fn send(&self, message: WriterMessage) {
        match self.tx.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!("response metrics writer stopped; sample discarded");
            }
        }
    }

    pub(crate) fn sampler(
        &self,
        start: &AgentStartPayload,
        settings: Option<&SessionSettingsValues>,
    ) -> ResponseSampler {
        ResponseSampler {
            metrics: self.clone(),
            labels: labels(start, settings),
            input_at: None,
            turn: None,
            open: None,
            awaiting_usage: Vec::new(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn flush(&self) -> PathBuf {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || tx.send(WriterMessage::Flush(done_tx)))
            .await
            .expect("response metrics flush task")
            .expect("response metrics writer running");
        done_rx.await.expect("response metrics writer flushed");
        (*self.path).clone()
    }
}

fn open(path: &Path) -> Result<Connection, String> {
    let fail = |error: rusqlite::Error| {
        format!(
            "response metrics database {} is unusable: {error}",
            path.display()
        )
    };
    let mut connection = Connection::open(path).map_err(fail)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(fail)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(format!(
            "response metrics database {} requires WAL mode",
            path.display()
        ));
    }
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(fail)?;
    let transaction = connection.transaction().map_err(fail)?;
    let version: u32 = transaction
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(fail)?;
    match version {
        0 => {
            transaction.execute_batch(&schema_sql()).map_err(fail)?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(fail)?;
        }
        SCHEMA_VERSION => {}
        other => {
            return Err(format!(
                "response metrics database {} has unknown schema version {other}",
                path.display()
            ));
        }
    }
    transaction.commit().map_err(fail)?;
    Ok(connection)
}

fn bucket_columns(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|index| format!("{prefix}{index}")).collect()
}

fn schema_sql() -> String {
    let histogram = bucket_columns("ttft_bucket_", TTFT_BUCKETS_MS.len() + 1)
        .into_iter()
        .chain(bucket_columns("tps_bucket_", TPS_BUCKETS.len() + 1))
        .map(|column| format!("{column} INTEGER NOT NULL DEFAULT 0"))
        .collect::<Vec<_>>()
        .join(",\n            ");
    format!(
        "CREATE TABLE response_samples (
            id INTEGER PRIMARY KEY,
            ended_at_ms INTEGER NOT NULL,
            backend TEXT NOT NULL,
            model TEXT,
            settings_key TEXT,
            settings_json TEXT,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            project_id TEXT,
            custom_agent_id TEXT,
            origin TEXT NOT NULL,
            is_child INTEGER NOT NULL,
            streamed INTEGER NOT NULL,
            ttft_from_input_ms INTEGER,
            ttft_from_start_ms INTEGER,
            generation_ms INTEGER,
            total_ms INTEGER,
            usage_known INTEGER NOT NULL,
            input_tokens INTEGER,
            cached_prompt_tokens INTEGER,
            cache_creation_input_tokens INTEGER,
            output_tokens INTEGER,
            reasoning_tokens INTEGER,
            output_tokens_per_second REAL,
            outcome TEXT NOT NULL
        );
        CREATE INDEX response_samples_backend_model_time
            ON response_samples (backend, model, ended_at_ms);
        CREATE INDEX response_samples_settings_time
            ON response_samples (settings_key, ended_at_ms);
        CREATE TABLE turn_samples (
            id INTEGER PRIMARY KEY,
            started_at_ms INTEGER NOT NULL,
            ended_at_ms INTEGER NOT NULL,
            backend TEXT NOT NULL,
            settings_key TEXT,
            settings_json TEXT,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            project_id TEXT,
            custom_agent_id TEXT,
            origin TEXT NOT NULL,
            is_child INTEGER NOT NULL,
            total_ms INTEGER NOT NULL,
            model_ms INTEGER NOT NULL,
            other_ms INTEGER NOT NULL,
            responses INTEGER NOT NULL,
            usage_complete INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL,
            cached_prompt_tokens INTEGER NOT NULL,
            cache_creation_input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            reasoning_tokens INTEGER NOT NULL,
            outcome TEXT NOT NULL
        );
        CREATE INDEX turn_samples_backend_time ON turn_samples (backend, ended_at_ms);
        CREATE INDEX turn_samples_settings_time ON turn_samples (settings_key, ended_at_ms);
        CREATE TABLE daily_rollups (
            day TEXT NOT NULL,
            backend TEXT NOT NULL,
            model TEXT NOT NULL,
            settings_key TEXT NOT NULL,
            responses INTEGER NOT NULL DEFAULT 0,
            streamed_responses INTEGER NOT NULL DEFAULT 0,
            usage_known_responses INTEGER NOT NULL DEFAULT 0,
            ttft_samples INTEGER NOT NULL DEFAULT 0,
            ttft_from_input_ms_sum INTEGER NOT NULL DEFAULT 0,
            tps_samples INTEGER NOT NULL DEFAULT 0,
            tps_output_tokens_sum INTEGER NOT NULL DEFAULT 0,
            tps_generation_ms_sum INTEGER NOT NULL DEFAULT 0,
            input_tokens_sum INTEGER NOT NULL DEFAULT 0,
            cached_prompt_tokens_sum INTEGER NOT NULL DEFAULT 0,
            output_tokens_sum INTEGER NOT NULL DEFAULT 0,
            {histogram},
            PRIMARY KEY (day, backend, model, settings_key)
        );
        CREATE TABLE writer_stats (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            dropped_samples INTEGER NOT NULL
        );
        INSERT INTO writer_stats (id, dropped_samples) VALUES (1, 0);"
    )
}

fn run_writer(mut connection: Connection, rx: Receiver<WriterMessage>, dropped: Arc<AtomicU64>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        let mut samples = Vec::with_capacity(batch.len());
        #[cfg(any(test, feature = "test-support"))]
        let mut flushes = Vec::new();
        for message in batch {
            match message {
                WriterMessage::Response(sample) => samples.push(Ok(sample)),
                WriterMessage::Turn(sample) => samples.push(Err(sample)),
                #[cfg(any(test, feature = "test-support"))]
                WriterMessage::Flush(done) => flushes.push(done),
            }
        }
        let newly_dropped = dropped.swap(0, Ordering::Relaxed);
        if newly_dropped > 0 {
            tracing::warn!(
                dropped = newly_dropped,
                "response metrics queue was full; samples dropped"
            );
        }
        if let Err(error) = write_batch(&mut connection, &samples, newly_dropped) {
            let lost = newly_dropped.saturating_add(samples.len() as u64);
            tracing::error!(%error, lost, "response metrics batch was not saved");
            dropped.fetch_add(lost, Ordering::Relaxed);
        }
        #[cfg(any(test, feature = "test-support"))]
        for done in flushes {
            let _ = done.send(());
        }
    }
}

type BatchEntry = Result<Box<ResponseSample>, Box<TurnSample>>;

fn write_batch(
    connection: &mut Connection,
    samples: &[BatchEntry],
    dropped: u64,
) -> Result<(), rusqlite::Error> {
    let transaction = connection.transaction()?;
    for sample in samples {
        match sample {
            Ok(response) => insert_response(&transaction, response)?,
            Err(turn) => insert_turn(&transaction, turn)?,
        }
    }
    if dropped > 0 {
        transaction.execute(
            "UPDATE writer_stats SET dropped_samples = dropped_samples + ?1 WHERE id = 1",
            params![dropped as i64],
        )?;
    }
    transaction.commit()
}

fn tokens_per_second(sample: &ResponseSample) -> Option<f64> {
    let usage = sample.usage.as_ref()?;
    let generation_ms = sample.generation_ms.filter(|ms| *ms > 0)?;
    (sample.streamed && sample.outcome == Outcome::Completed)
        .then(|| usage.output_tokens as f64 * 1_000.0 / generation_ms as f64)
}

fn day_of(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    // Civil-from-days (Howard Hinnant), UTC.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn insert_response(transaction: &Transaction<'_>, sample: &ResponseSample) -> rusqlite::Result<()> {
    let labels = &sample.labels;
    let usage = sample.usage.as_ref();
    let tps = tokens_per_second(sample);
    transaction
        .prepare_cached(
            "INSERT INTO response_samples (
                ended_at_ms, backend, model, settings_key, settings_json, agent_id, session_id,
                project_id, custom_agent_id, origin, is_child, streamed, ttft_from_input_ms,
                ttft_from_start_ms, generation_ms, total_ms, usage_known, input_tokens,
                cached_prompt_tokens, cache_creation_input_tokens, output_tokens,
                reasoning_tokens, output_tokens_per_second, outcome
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        )?
        .execute(params![
            sample.ended_at_ms as i64,
            labels.backend,
            sample.model,
            labels.settings_key,
            labels.settings_json,
            labels.agent_id,
            labels.session_id,
            labels.project_id,
            labels.custom_agent_id,
            labels.origin,
            labels.is_child,
            sample.streamed,
            sample.ttft_from_input_ms.map(|ms| ms as i64),
            sample.ttft_from_start_ms.map(|ms| ms as i64),
            sample.generation_ms.map(|ms| ms as i64),
            sample.total_ms.map(|ms| ms as i64),
            usage.is_some(),
            usage.map(|usage| usage.input_tokens as i64),
            usage.and_then(|usage| usage.cached_prompt_tokens.map(|value| value as i64)),
            usage.and_then(|usage| usage.cache_creation_input_tokens.map(|value| value as i64)),
            usage.map(|usage| usage.output_tokens as i64),
            usage.and_then(|usage| usage.reasoning_tokens.map(|value| value as i64)),
            tps,
            sample.outcome.as_str(),
        ])?;

    let ttft = sample.ttft_from_input_ms;
    let ttft_bucket = ttft.map(|ms| {
        TTFT_BUCKETS_MS
            .iter()
            .position(|limit| ms <= *limit)
            .unwrap_or(TTFT_BUCKETS_MS.len())
    });
    let tps_bucket = tps.map(|value| {
        TPS_BUCKETS
            .iter()
            .position(|limit| value <= *limit)
            .unwrap_or(TPS_BUCKETS.len())
    });
    let mut increments = String::new();
    if let Some(bucket) = ttft_bucket {
        increments.push_str(&format!(
            ", ttft_bucket_{bucket} = ttft_bucket_{bucket} + 1"
        ));
    }
    if let Some(bucket) = tps_bucket {
        increments.push_str(&format!(", tps_bucket_{bucket} = tps_bucket_{bucket} + 1"));
    }
    transaction.execute(
        "INSERT OR IGNORE INTO daily_rollups (day, backend, model, settings_key)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            day_of(sample.ended_at_ms),
            labels.backend,
            sample.model.as_deref().unwrap_or(""),
            labels.settings_key.as_deref().unwrap_or(""),
        ],
    )?;
    let tps_tokens = tps.and(usage).map(|usage| usage.output_tokens).unwrap_or(0);
    let tps_ms = tps.and(sample.generation_ms).unwrap_or(0);
    transaction
        .prepare_cached(&format!(
            "UPDATE daily_rollups SET
                responses = responses + 1,
                streamed_responses = streamed_responses + ?5,
                usage_known_responses = usage_known_responses + ?6,
                ttft_samples = ttft_samples + ?7,
                ttft_from_input_ms_sum = ttft_from_input_ms_sum + ?8,
                tps_samples = tps_samples + ?9,
                tps_output_tokens_sum = tps_output_tokens_sum + ?10,
                tps_generation_ms_sum = tps_generation_ms_sum + ?11,
                input_tokens_sum = input_tokens_sum + ?12,
                cached_prompt_tokens_sum = cached_prompt_tokens_sum + ?13,
                output_tokens_sum = output_tokens_sum + ?14
                {increments}
             WHERE day = ?1 AND backend = ?2 AND model = ?3 AND settings_key = ?4"
        ))?
        .execute(params![
            day_of(sample.ended_at_ms),
            labels.backend,
            sample.model.as_deref().unwrap_or(""),
            labels.settings_key.as_deref().unwrap_or(""),
            sample.streamed as i64,
            usage.is_some() as i64,
            ttft.is_some() as i64,
            ttft.unwrap_or(0) as i64,
            tps.is_some() as i64,
            tps_tokens as i64,
            tps_ms as i64,
            usage.map(|usage| usage.input_tokens).unwrap_or(0) as i64,
            usage
                .and_then(|usage| usage.cached_prompt_tokens)
                .unwrap_or(0) as i64,
            usage.map(|usage| usage.output_tokens).unwrap_or(0) as i64,
        ])?;
    Ok(())
}

fn insert_turn(transaction: &Transaction<'_>, sample: &TurnSample) -> rusqlite::Result<()> {
    let labels = &sample.labels;
    transaction
        .prepare_cached(
            "INSERT INTO turn_samples (
                started_at_ms, ended_at_ms, backend, settings_key, settings_json, agent_id,
                session_id, project_id, custom_agent_id, origin, is_child, total_ms, model_ms,
                other_ms, responses, usage_complete, input_tokens, cached_prompt_tokens,
                cache_creation_input_tokens, output_tokens, reasoning_tokens, outcome
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22)",
        )?
        .execute(params![
            sample.started_at_ms as i64,
            sample.ended_at_ms as i64,
            labels.backend,
            labels.settings_key,
            labels.settings_json,
            labels.agent_id,
            labels.session_id,
            labels.project_id,
            labels.custom_agent_id,
            labels.origin,
            labels.is_child,
            sample.total_ms as i64,
            sample.model_ms as i64,
            sample.total_ms.saturating_sub(sample.model_ms) as i64,
            sample.responses,
            sample.usage_complete,
            sample.usage.input_tokens as i64,
            sample.usage.cached_prompt_tokens.unwrap_or(0) as i64,
            sample.usage.cache_creation_input_tokens.unwrap_or(0) as i64,
            sample.usage.output_tokens as i64,
            sample.usage.reasoning_tokens.unwrap_or(0) as i64,
            sample.outcome.as_str(),
        ])?;
    Ok(())
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_millis() as u64
}

fn elapsed_ms(from: Instant, to: Instant) -> u64 {
    to.saturating_duration_since(from).as_millis() as u64
}

fn unit_variant_label<T: serde::Serialize + std::fmt::Debug>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(label)) => label,
        other => panic!("{value:?} must serialize as a unit variant, got {other:?}"),
    }
}

fn settings_labels(settings: &SessionSettingsValues) -> (String, String) {
    let mut entries: Vec<_> = settings.0.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    let key = entries
        .iter()
        .map(|(name, value)| {
            let value = match value {
                SessionSettingValue::String(text) => text.clone(),
                SessionSettingValue::Bool(flag) => flag.to_string(),
                SessionSettingValue::Integer(number) => number.to_string(),
                SessionSettingValue::Null => "null".to_owned(),
            };
            format!("{name}={value}")
        })
        .collect::<Vec<_>>()
        .join(",");
    let ordered: serde_json::Map<String, serde_json::Value> = entries
        .into_iter()
        .map(|(name, value)| {
            (
                name.clone(),
                serde_json::to_value(value).expect("session setting values serialize"),
            )
        })
        .collect();
    (key, serde_json::Value::Object(ordered).to_string())
}

fn labels(start: &AgentStartPayload, settings: Option<&SessionSettingsValues>) -> Labels {
    let (settings_key, settings_json) = match settings.map(settings_labels) {
        Some((key, json)) => (Some(key), Some(json)),
        None => (None, None),
    };
    Labels {
        backend: unit_variant_label(&start.backend_kind),
        settings_key,
        settings_json,
        agent_id: start.agent_id.0.clone(),
        session_id: start.session_id.as_ref().map(|id| id.0.clone()),
        project_id: start.project_id.as_ref().map(|id| id.0.clone()),
        custom_agent_id: start.custom_agent_id.as_ref().map(|id| id.0.clone()),
        origin: unit_variant_label(&start.origin),
        is_child: start.parent_agent_id.is_some(),
    }
}

fn request_usage(message: &ChatMessage) -> Option<TokenUsage> {
    message
        .token_usage
        .as_ref()
        .and_then(|usage| usage.request.known_usage())
        .cloned()
}

fn add_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    let add = |total: &mut Option<u64>, value: Option<u64>| {
        *total = Some(total.unwrap_or(0).saturating_add(value.unwrap_or(0)));
    };
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    add(&mut total.cached_prompt_tokens, usage.cached_prompt_tokens);
    add(
        &mut total.cache_creation_input_tokens,
        usage.cache_creation_input_tokens,
    );
    add(&mut total.reasoning_tokens, usage.reasoning_tokens);
}

struct TurnState {
    labels: Labels,
    started_at: Instant,
    started_wall_ms: u64,
    responses: u32,
    model_ms: u64,
    usage: TokenUsage,
    usage_complete: bool,
    outcome: Outcome,
}

struct OpenResponse {
    started_at: Instant,
    input_at: Option<Instant>,
    first_token_at: Option<Instant>,
    model: Option<String>,
}

struct AwaitingUsage {
    message_id: String,
    sample: ResponseSample,
}

/// Per-agent, actor-owned fold of live chat events into samples. Deltas only
/// stamp an instant; everything else waits for a response or turn boundary.
pub(crate) struct ResponseSampler {
    metrics: ResponseMetrics,
    labels: Labels,
    input_at: Option<Instant>,
    turn: Option<TurnState>,
    open: Option<OpenResponse>,
    awaiting_usage: Vec<AwaitingUsage>,
}

impl ResponseSampler {
    /// Labels apply from the next turn; a running turn keeps the labels it
    /// started with.
    pub(crate) fn set_labels(
        &mut self,
        start: &AgentStartPayload,
        settings: Option<&SessionSettingsValues>,
    ) {
        self.labels = labels(start, settings);
    }

    fn current_labels(&self) -> Labels {
        self.turn
            .as_ref()
            .map_or_else(|| self.labels.clone(), |turn| turn.labels.clone())
    }

    pub(crate) fn observe(&mut self, event: &ChatEvent) {
        let now = Instant::now();
        match event {
            ChatEvent::TypingStatusChanged(true) => {
                if self.turn.is_none() {
                    self.turn = Some(TurnState {
                        labels: self.labels.clone(),
                        started_at: now,
                        started_wall_ms: wall_ms(),
                        responses: 0,
                        model_ms: 0,
                        usage: TokenUsage::default(),
                        usage_complete: true,
                        outcome: Outcome::Completed,
                    });
                    self.input_at = Some(now);
                }
            }
            ChatEvent::TypingStatusChanged(false) => self.finish_turn(now),
            ChatEvent::ToolExecutionCompleted(_) => self.input_at = Some(now),
            ChatEvent::MessageAdded(message) => match &message.sender {
                MessageSender::User => self.input_at = Some(now),
                MessageSender::Error => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.outcome = Outcome::Error;
                    }
                }
                MessageSender::Assistant { .. } if self.open.is_none() => {
                    let input_at = self.input_at;
                    self.finish_response(
                        message,
                        OpenResponse {
                            started_at: input_at.unwrap_or(now),
                            input_at,
                            first_token_at: None,
                            model: None,
                        },
                        false,
                        now,
                    );
                }
                _ => {}
            },
            ChatEvent::OperationCancelled(_) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.outcome = Outcome::Cancelled;
                }
            }
            ChatEvent::StreamStart(data) => {
                self.open = Some(OpenResponse {
                    started_at: now,
                    input_at: self.input_at,
                    first_token_at: None,
                    model: data.model.clone().filter(|model| !model.trim().is_empty()),
                });
            }
            ChatEvent::StreamDelta(delta) | ChatEvent::StreamReasoningDelta(delta) => {
                if let Some(open) = self.open.as_mut()
                    && open.first_token_at.is_none()
                    && !delta.text.is_empty()
                {
                    open.first_token_at = Some(now);
                }
            }
            ChatEvent::StreamEnd(data) => {
                if let Some(open) = self.open.take() {
                    self.finish_response(&data.message, open, true, now);
                }
            }
            ChatEvent::MessageMetadataUpdated(update) => {
                let usage = update
                    .token_usage
                    .as_ref()
                    .and_then(|usage| usage.request.known_usage());
                if let Some(usage) = usage
                    && let Some(index) = self
                        .awaiting_usage
                        .iter()
                        .position(|entry| entry.message_id == update.message_id.0)
                {
                    let mut sample = self.awaiting_usage.swap_remove(index).sample;
                    if let Some(model) = update.model_info.as_ref() {
                        sample.model = Some(model.model.clone());
                    }
                    sample.usage = Some(usage.clone());
                    self.emit_response(sample);
                }
            }
            _ => {}
        }
    }

    fn finish_response(
        &mut self,
        message: &ChatMessage,
        open: OpenResponse,
        streamed: bool,
        now: Instant,
    ) {
        let first_token_at = open.first_token_at;
        let sample = ResponseSample {
            labels: self.current_labels(),
            model: message
                .model_info
                .as_ref()
                .map(|info| info.model.clone())
                .or(open.model),
            ended_at_ms: wall_ms(),
            streamed: streamed && first_token_at.is_some(),
            ttft_from_input_ms: first_token_at
                .zip(open.input_at)
                .map(|(first, input)| elapsed_ms(input, first)),
            ttft_from_start_ms: first_token_at.map(|first| elapsed_ms(open.started_at, first)),
            generation_ms: first_token_at.map(|first| elapsed_ms(first, now)),
            total_ms: open.input_at.map(|input| elapsed_ms(input, now)),
            usage: request_usage(message),
            outcome: Outcome::Completed,
        };
        if let Some(turn) = self.turn.as_mut() {
            turn.responses = turn.responses.saturating_add(1);
            turn.model_ms = turn
                .model_ms
                .saturating_add(elapsed_ms(open.started_at, now));
        }
        self.input_at = Some(now);
        match (&sample.usage, &message.message_id) {
            (None, Some(message_id)) => self.awaiting_usage.push(AwaitingUsage {
                message_id: message_id.0.clone(),
                sample,
            }),
            _ => self.emit_response(sample),
        }
    }

    fn emit_response(&mut self, sample: ResponseSample) {
        if let Some(turn) = self.turn.as_mut() {
            match &sample.usage {
                Some(usage) => add_usage(&mut turn.usage, usage),
                None => turn.usage_complete = false,
            }
        }
        self.metrics.send(WriterMessage::Response(Box::new(sample)));
    }

    fn finish_turn(&mut self, now: Instant) {
        if let Some(open) = self.open.take() {
            let outcome = match self.turn.as_ref().map(|turn| turn.outcome) {
                Some(Outcome::Completed) | None => Outcome::Interrupted,
                Some(outcome) => outcome,
            };
            let first_token_at = open.first_token_at;
            if let Some(turn) = self.turn.as_mut() {
                turn.responses = turn.responses.saturating_add(1);
                turn.model_ms = turn
                    .model_ms
                    .saturating_add(elapsed_ms(open.started_at, now));
            }
            self.emit_response(ResponseSample {
                labels: self.current_labels(),
                model: open.model,
                ended_at_ms: wall_ms(),
                streamed: first_token_at.is_some(),
                ttft_from_input_ms: first_token_at
                    .zip(open.input_at)
                    .map(|(first, input)| elapsed_ms(input, first)),
                ttft_from_start_ms: first_token_at.map(|first| elapsed_ms(open.started_at, first)),
                generation_ms: first_token_at.map(|first| elapsed_ms(first, now)),
                total_ms: open.input_at.map(|input| elapsed_ms(input, now)),
                usage: None,
                outcome,
            });
        }
        for entry in std::mem::take(&mut self.awaiting_usage) {
            self.emit_response(entry.sample);
        }
        self.input_at = None;
        let Some(turn) = self.turn.take() else {
            return;
        };
        self.metrics.send(WriterMessage::Turn(Box::new(TurnSample {
            labels: turn.labels,
            started_at_ms: turn.started_wall_ms,
            ended_at_ms: wall_ms(),
            total_ms: elapsed_ms(turn.started_at, now),
            model_ms: turn.model_ms,
            responses: turn.responses,
            usage_complete: turn.usage_complete,
            usage: turn.usage,
            outcome: turn.outcome,
        })));
    }
}
