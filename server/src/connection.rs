use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::{
    EncodedRecord, Envelope, FrameError, FrameKind, FrameReader, ProtocolFrame, encode_frame,
};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::Connection;
use crate::error::AppError;
use crate::host::{AgentReplayMode, HostHandle};
use crate::router::route_client_envelope;
use crate::stream::{OutputLane, OutputQueue, QueuedOutput, SchedulerToken, Stream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionOrigin {
    Desktop,
    Mobile,
}

const BOOTSTRAP_REPLAY_GRACE: std::time::Duration = std::time::Duration::from_millis(50);
const MOBILE_CLIENT_LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
static INBOUND_AUDIO_LOG_SAMPLE: AtomicU64 = AtomicU64::new(0);
static OUTBOUND_AUDIO_LOG_SAMPLE: AtomicU64 = AtomicU64::new(0);

fn has_peer_liveness_deadline(origin: ConnectionOrigin) -> bool {
    origin == ConnectionOrigin::Mobile
}

struct AppLoopResources {
    host: HostHandle,
    host_stream: protocol::StreamPath,
    host_output_stream: Stream,
    inbound_rx: mpsc::Receiver<ProtocolFrame>,
    incoming_seq: protocol::SeqValidator,
    cancel: CancellationToken,
    origin: ConnectionOrigin,
    first_request: Arc<Notify>,
    voice_provider: Arc<dyn crate::voice::NovaProvider>,
}

#[derive(Default)]
struct VoiceIngressState {
    latest_generation: Option<u64>,
    current_session: Option<(u64, protocol::VoiceSessionId)>,
    stale_session: Option<(u64, protocol::VoiceSessionId)>,
}

impl VoiceIngressState {
    fn should_ignore_stale_audio(&self, envelope: &Envelope, binary: &[u8]) -> bool {
        if envelope.kind != FrameKind::VoiceAudio {
            return false;
        }
        let Ok(payload) = envelope.parse_payload::<protocol::VoiceAudioPayload>() else {
            return false;
        };
        let Some((generation, session_id)) = &self.stale_session else {
            return false;
        };
        payload.generation == *generation
            && payload.session_id == *session_id
            && payload.direction == protocol::VoiceDirection::Input
            && payload.validate_body(binary.len()).is_ok()
            && payload.timestamp_samples_48k == payload.first_media_seq.saturating_mul(960)
            && envelope.stream.0.strip_prefix("/voice/") == Some(payload.session_id.0.as_str())
    }

    fn observe_validated(&mut self, envelope: &Envelope) {
        if envelope.kind == FrameKind::VoiceStart {
            if let Ok(start) = envelope.parse_payload::<protocol::VoiceStartPayload>()
                && self
                    .latest_generation
                    .is_none_or(|generation| start.generation > generation)
            {
                self.stale_session = self.current_session.take();
                self.latest_generation = Some(start.generation);
            }
            return;
        }
        let identity = match envelope.kind {
            FrameKind::VoiceAudio => envelope
                .parse_payload::<protocol::VoiceAudioPayload>()
                .ok()
                .map(|payload| (payload.generation, payload.session_id)),
            FrameKind::VoiceInputEnd | FrameKind::VoiceInterrupt => envelope
                .parse_payload::<protocol::VoiceSessionPayload>()
                .ok()
                .map(|payload| (payload.generation, payload.session_id)),
            FrameKind::VoiceStop => envelope
                .parse_payload::<protocol::VoiceStopPayload>()
                .ok()
                .map(|payload| (payload.generation, payload.session_id)),
            _ => None,
        };
        if let Some(identity) = identity
            && self.latest_generation == Some(identity.0)
            && self
                .current_session
                .as_ref()
                .is_none_or(|current| current == &identity)
        {
            self.current_session = Some(identity);
        }
    }
}

pub async fn run_connection(connection: Connection, host: HostHandle) -> Result<(), FrameError> {
    run_connection_with_origin(
        connection,
        host,
        ConnectionOrigin::Desktop,
        Arc::new(crate::voice_aws::AwsNovaProvider),
    )
    .await
}

pub async fn run_mobile_connection(
    connection: Connection,
    host: HostHandle,
) -> Result<(), FrameError> {
    run_connection_with_origin(
        connection,
        host,
        ConnectionOrigin::Mobile,
        Arc::new(crate::voice_aws::AwsNovaProvider),
    )
    .await
}

#[cfg(feature = "test-support")]
pub async fn run_connection_with_synthetic_voice(
    connection: Connection,
    host: HostHandle,
) -> Result<(), FrameError> {
    run_connection_with_origin(
        connection,
        host,
        ConnectionOrigin::Desktop,
        Arc::new(crate::voice::SyntheticNovaProvider),
    )
    .await
}

async fn run_connection_with_origin(
    connection: Connection,
    host: HostHandle,
    origin: ConnectionOrigin,
    voice_provider: Arc<dyn crate::voice::NovaProvider>,
) -> Result<(), FrameError> {
    let host_stream = connection
        .outgoing_seq
        .keys()
        .find(|stream| stream.0.starts_with("/host/"))
        .cloned()
        .expect("missing /host/<uuid> stream in connection outgoing sequence map");

    let Connection {
        reader,
        writer,
        incoming_seq,
        outgoing_seq,
    } = connection;

    let output_queue = OutputQueue::default();
    let (inbound_tx, inbound_rx) = mpsc::channel::<ProtocolFrame>(256);

    let host_output_stream = Stream::new(host_stream.clone(), output_queue.clone());

    let agent_replay = match origin {
        ConnectionOrigin::Desktop => AgentReplayMode::Eager,
        ConnectionOrigin::Mobile => AgentReplayMode::Lazy,
    };
    let deferred_attachments = host
        .register_connection_host_stream(
            host_output_stream.clone(),
            agent_replay,
            origin == ConnectionOrigin::Desktop,
        )
        .await;
    let host_for_attachments = host.clone();
    tokio::spawn(async move {
        for attachment in deferred_attachments {
            host_for_attachments
                .attach_deferred_agent_stream(attachment)
                .await;
        }
    });

    let cancel = CancellationToken::new();

    let reader_task = {
        let cancel = cancel.clone();
        tokio::spawn(reader_loop(reader, inbound_tx, cancel))
    };
    let writer_task = {
        let cancel = cancel.clone();
        tokio::spawn(writer_loop(
            writer,
            output_queue.clone(),
            outgoing_seq,
            cancel,
        ))
    };
    let first_request = Arc::new(Notify::new());
    let app_task = {
        let cancel = cancel.clone();
        let host = host.clone();
        let host_stream = host_stream.clone();
        let host_output_stream = host_output_stream.clone();
        let app_first_request = Arc::clone(&first_request);
        tokio::spawn(async move {
            app_loop(AppLoopResources {
                host,
                host_stream,
                host_output_stream,
                inbound_rx,
                incoming_seq,
                cancel,
                origin,
                first_request: app_first_request,
                voice_provider,
            })
            .await
        })
    };

    let capacity_replay_host = host.clone();
    let capacity_replay_stream = host_stream.clone();
    let capacity_replay_cancel = cancel.clone();
    let capacity_replay_task = tokio::spawn(async move {
        tokio::select! {
            _ = capacity_replay_cancel.cancelled() => return,
            _ = first_request.notified() => {}
            _ = tokio::time::sleep(BOOTSTRAP_REPLAY_GRACE) => {}
        }
        capacity_replay_host
            .replay_backend_capacity_for_host_stream(&capacity_replay_stream)
            .await;
    });

    // Wait for the first task to finish, then tear the scope down.
    // Drain only the two losers — re-polling a JoinHandle that tokio::select!
    // already resolved panics with "JoinHandle polled after completion".
    enum SelectWinner {
        Reader,
        Writer,
        App,
    }
    let mut reader_task = reader_task;
    let mut writer_task = writer_task;
    let mut app_task = app_task;
    let (result, winner) = tokio::select! {
        res = &mut reader_task => {
            cancel.cancel();
            writer_task.abort();
            app_task.abort();
            (res.unwrap_or(Ok(())), SelectWinner::Reader)
        }
        res = &mut writer_task => {
            cancel.cancel();
            reader_task.abort();
            app_task.abort();
            (res.unwrap_or(Ok(())), SelectWinner::Writer)
        }
        res = &mut app_task => {
            cancel.cancel();
            reader_task.abort();
            writer_task.abort();
            (res.unwrap_or(Ok(())), SelectWinner::App)
        }
    };

    // Drain the two aborted tasks so their Drop runs (socket halves close, fd released).
    match winner {
        SelectWinner::Reader => {
            let _ = writer_task.await;
            let _ = app_task.await;
        }
        SelectWinner::Writer => {
            let _ = reader_task.await;
            let _ = app_task.await;
        }
        SelectWinner::App => {
            let _ = reader_task.await;
            let _ = writer_task.await;
        }
    }
    capacity_replay_task.abort();
    let _ = capacity_replay_task.await;

    host.unregister_host_stream(&host_stream).await;
    normalize_peer_disconnect(result)
}

fn normalize_peer_disconnect(result: Result<(), FrameError>) -> Result<(), FrameError> {
    match result {
        Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

async fn reader_loop<R>(
    reader: R,
    inbound_tx: mpsc::Sender<ProtocolFrame>,
    cancel: CancellationToken,
) -> Result<(), FrameError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    let mut reader = FrameReader::new(reader);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            read = reader.read_frame() => {
                match read? {
                    Some(envelope) => {
                        if inbound_tx.send(envelope).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

async fn writer_loop<W>(
    mut writer: W,
    output: OutputQueue,
    mut outgoing_seq: std::collections::HashMap<protocol::StreamPath, u64>,
    cancel: CancellationToken,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut pending_records = std::collections::VecDeque::<EncodedRecord>::new();
    let mut pending_record_stream: Option<protocol::StreamPath> = None;
    let mut pending_record_completions = Vec::<SchedulerToken>::new();
    let mut deferred_same_stream = std::collections::VecDeque::<QueuedOutput>::new();
    let mut deferred_same_stream_bytes = 0usize;
    let mut interleaved_audio_streak = 0u8;
    let mut message_id = 1u64;
    enum WriterNext {
        Output(QueuedOutput),
        LogicalChunk {
            record: EncodedRecord,
            final_record: bool,
        },
        Closed,
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            next = async {
                if pending_records.is_empty()
                    && let Some(item) = deferred_same_stream.pop_front()
                {
                    deferred_same_stream_bytes =
                        deferred_same_stream_bytes.saturating_sub(item.bytes);
                    return Ok(WriterNext::Output(item));
                }
                if let Some(item) = output
                    .try_pop_urgent(
                        pending_records.is_empty() || interleaved_audio_streak < 8,
                    )?
                {
                    if pending_record_stream
                        .as_ref()
                        .is_some_and(|stream| stream == &item.frame.envelope.stream)
                    {
                        if deferred_same_stream.len() >= 64
                            || deferred_same_stream_bytes.saturating_add(item.bytes)
                                > 8 * 1024 * 1024
                        {
                            return Err(crate::stream::StreamClosed);
                        }
                        deferred_same_stream_bytes =
                            deferred_same_stream_bytes.saturating_add(item.bytes);
                        deferred_same_stream.push_back(item);
                    } else {
                        if item.lane == OutputLane::Audio {
                            interleaved_audio_streak =
                                interleaved_audio_streak.saturating_add(1);
                        }
                        return Ok(WriterNext::Output(item));
                    }
                }
                if let Some(chunk) = pending_records.pop_front() {
                    interleaved_audio_streak = 0;
                    let final_record = pending_records.is_empty();
                    return Ok(WriterNext::LogicalChunk {
                        record: chunk,
                        final_record,
                    });
                }
                Ok(match output.pop().await? {
                    Some(item) => WriterNext::Output(item),
                    None => WriterNext::Closed,
                })
            } => {
                let next: Result<WriterNext, crate::stream::StreamClosed> = next;
                let next = next.map_err(|_| {
                    FrameError::Protocol("connection control lane overflow".into())
                })?;
                let WriterNext::Output(mut queued) = next else {
                    match next {
                        WriterNext::LogicalChunk {
                            record,
                            final_record,
                        } => {
                            writer.write_all(&record.bytes).await?;
                            output.record_written();
                            if final_record {
                                output.complete_tokens(&pending_record_completions);
                                pending_record_completions.clear();
                                pending_record_stream = None;
                                writer.flush().await?;
                            } else {
                                tokio::task::yield_now().await;
                            }
                            continue;
                        }
                        WriterNext::Closed => return Ok(()),
                        WriterNext::Output(_) => unreachable!(),
                    }
                };
                let interleaved = !pending_records.is_empty();
                assign_sequence(&mut outgoing_seq, &mut queued.frame.envelope);
                log_outgoing(&queued.frame.envelope);
                let mut records = encode_frame(&queued.frame, message_id)?;
                message_id = message_id.wrapping_add(1).max(1);
                let first = records.remove(0);
                writer.write_all(&first.bytes).await?;
                output.record_written();
                if records.is_empty() {
                    output.complete(&queued);
                } else {
                    if interleaved {
                        return Err(FrameError::Protocol(
                            "fragmented urgent frame cannot replace in-flight reassembly".into(),
                        ));
                    }
                    interleaved_audio_streak = 0;
                    pending_record_stream = Some(queued.frame.envelope.stream.clone());
                    pending_record_completions.clone_from(&queued.completions);
                    pending_records.extend(records);
                    tokio::task::yield_now().await;
                }
                if interleaved || pending_records.is_empty() {
                    writer.flush().await?;
                }
            }
        }
    }
}

fn assign_sequence(
    outgoing_seq: &mut std::collections::HashMap<protocol::StreamPath, u64>,
    envelope: &mut Envelope,
) {
    let seq = outgoing_seq.get(&envelope.stream).copied().unwrap_or(0);
    outgoing_seq.insert(envelope.stream.clone(), seq + 1);
    envelope.seq = seq;
    if envelope.stream.0.starts_with("/agent/") {
        if envelope.seq == 0 && envelope.kind != FrameKind::AgentBootstrap {
            tracing::warn!(
                stream = %envelope.stream,
                seq = envelope.seq,
                kind = %envelope.kind,
                "agent stream first envelope is not AgentBootstrap"
            );
        }
        if envelope.kind == FrameKind::AgentBootstrap && envelope.seq != 0 {
            tracing::error!(
                stream = %envelope.stream,
                seq = envelope.seq,
                kind = %envelope.kind,
                "AgentBootstrap assigned nonzero sequence"
            );
        }
    }
}

fn log_outgoing(envelope: &Envelope) {
    if envelope.kind == FrameKind::VoiceAudio {
        let count = OUTBOUND_AUDIO_LOG_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_multiple_of(256) {
            tracing::debug!(
                stream = %envelope.stream,
                seq = envelope.seq,
                sample_count = count,
                suppressed = 255,
                "server voice audio output sample"
            );
        }
    } else if is_high_volume_code_intel_frame(envelope.kind) {
        tracing::debug!(
            stream = %envelope.stream,
            seq = envelope.seq,
            kind = %envelope.kind,
            "server sending high-volume code-intel envelope"
        );
    } else {
        tracing::info!(
            stream = %envelope.stream,
            seq = envelope.seq,
            kind = %envelope.kind,
            "server sending envelope"
        );
    }
}

#[cfg(feature = "test-support")]
pub struct ProductionWriterProbe {
    stream: Stream,
    queue: OutputQueue,
}

#[cfg(feature = "test-support")]
impl ProductionWriterProbe {
    pub fn send(
        &self,
        path: protocol::StreamPath,
        kind: FrameKind,
        payload: serde_json::Value,
        binary: Vec<u8>,
    ) -> Result<(), String> {
        self.stream
            .with_path(path)
            .send_binary(kind, payload, binary)
            .map_err(|_| "production writer queue closed".to_owned())
    }

    pub fn close(&self) {
        self.queue.close();
    }
    pub fn records_written(&self) -> u64 {
        self.queue.records_written()
    }
}

#[cfg(feature = "test-support")]
pub fn start_production_writer_probe(
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    initial_sequences: std::collections::HashMap<protocol::StreamPath, u64>,
) -> (
    ProductionWriterProbe,
    tokio::task::JoinHandle<Result<(), FrameError>>,
) {
    let queue = OutputQueue::default();
    let stream = Stream::new(protocol::StreamPath("/host/test".into()), queue.clone());
    let task_queue = queue.clone();
    let task = tokio::spawn(async move {
        writer_loop(
            writer,
            task_queue,
            initial_sequences,
            CancellationToken::new(),
        )
        .await
    });
    (ProductionWriterProbe { stream, queue }, task)
}

async fn app_loop(resources: AppLoopResources) -> Result<(), FrameError> {
    let AppLoopResources {
        host,
        host_stream,
        host_output_stream,
        mut inbound_rx,
        mut incoming_seq,
        cancel,
        origin,
        first_request,
        voice_provider,
    } = resources;
    let (voice_tx, mut voice_rx) = mpsc::channel::<ProtocolFrame>(32);
    let voice_host = host.clone();
    let voice_output = host_output_stream.clone();
    let voice_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut voice =
            crate::voice::VoiceConnection::new(voice_host, voice_output, voice_provider);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = voice_cancel.cancelled() => {
                    voice.shutdown();
                    break;
                }
                _ = tick.tick() => {
                    if voice.drain_output().await.is_err() {
                        voice.shutdown();
                    }
                }
                command = voice_rx.recv() => match command {
                    Some(frame) => {
                        if voice.handle(frame).await.is_err() {
                            voice.fail_invalid_frame();
                        }
                    },
                    None => break,
                }
            }
        }
    });
    let mut voice_ingress = VoiceIngressState::default();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(MOBILE_CLIENT_LIVENESS_TIMEOUT),
                if has_peer_liveness_deadline(origin) =>
            {
                tracing::warn!(
                    stream = %host_stream,
                    timeout_ms = MOBILE_CLIENT_LIVENESS_TIMEOUT.as_millis(),
                    "closing mobile connection after client liveness timeout",
                );
                return Ok(());
            }
            maybe_frame = inbound_rx.recv() => {
                let Some(frame) = maybe_frame else {
                    return Ok(());
                };
                let ProtocolFrame { envelope, binary } = frame;

                if envelope.kind == FrameKind::VoiceAudio {
                    let count = INBOUND_AUDIO_LOG_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
                    if count.is_multiple_of(256) {
                        tracing::debug!(
                            stream = %envelope.stream,
                            seq = envelope.seq,
                            sample_count = count,
                            suppressed = 255,
                            "server voice audio input sample"
                        );
                    }
                } else if is_high_volume_code_intel_frame(envelope.kind) {
                    tracing::debug!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        "server received high-volume code-intel envelope"
                    );
                } else {
                    tracing::info!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        "server received envelope"
                    );
                }

                if let Err(error) =
                    incoming_seq.validate(&envelope.stream, envelope.seq, envelope.kind)
                {
                    tracing::warn!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        %error,
                        "closing connection after protocol violation",
                    );
                    return Err(error.into());
                }

                if voice_ingress.should_ignore_stale_audio(&envelope, &binary) {
                    tracing::debug!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        "ignored isolated stale-generation voice audio after sequence admission"
                    );
                    continue;
                }

                if envelope.stream.0.starts_with("/voice") {
                    voice_ingress.observe_validated(&envelope);
                    voice_tx
                        .send(ProtocolFrame { envelope, binary })
                        .await
                        .map_err(|_| FrameError::Protocol("voice actor closed".into()))?;
                    first_request.notify_one();
                    continue;
                }

                let request_stream = envelope.stream.clone();
                let request_kind = envelope.kind;
                if origin == ConnectionOrigin::Mobile
                    && is_terminal_control_command(request_kind)
                {
                    let error = AppError::invalid(
                        "mobile_terminal_command",
                        "terminal commands are not allowed from Tyde Mobile",
                    );
                    emit_command_error(
                        &host_output_stream,
                        request_stream,
                        request_kind,
                        &error,
                    );
                    first_request.notify_one();
                    continue;
                }

                let route_result =
                    route_client_envelope(&host, &host_stream, &host_output_stream, envelope)
                        .await;
                if let Err(error) = route_result {
                    emit_command_error(
                        &host_output_stream,
                        request_stream,
                        request_kind,
                        &error,
                    );

                    if error.fatal {
                        return Ok(());
                    }
                }
                first_request.notify_one();
            }
        }
    }
}

fn is_terminal_control_command(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::TerminalCreate
            | FrameKind::TerminalSend
            | FrameKind::TerminalResize
            | FrameKind::TerminalClose
    )
}

fn is_high_volume_code_intel_frame(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::CodeIntelSubscribeFile
            | FrameKind::CodeIntelUnsubscribeFile
            | FrameKind::CodeIntelSetVisibleRange
            | FrameKind::CodeIntelHover
            | FrameKind::CodeIntelNavigate
            | FrameKind::CodeIntelFindReferences
            | FrameKind::CodeIntelCancelReferences
            | FrameKind::CodeIntelOverview
            | FrameKind::CodeIntelStatus
            | FrameKind::CodeIntelFileModel
            | FrameKind::CodeIntelDiagnostics
            | FrameKind::CodeIntelHoverResult
            | FrameKind::CodeIntelNavigateResult
            | FrameKind::CodeIntelReferencesResults
            | FrameKind::CodeIntelReferencesComplete
    )
}

pub(crate) fn emit_command_error(
    host_output_stream: &Stream,
    request_stream: protocol::StreamPath,
    request_kind: FrameKind,
    error: &AppError,
) {
    if let Some(source) = error.source.as_ref() {
        tracing::warn!(
            operation = error.operation,
            request_kind = %request_kind,
            request_stream = %request_stream,
            code = ?error.kind,
            fatal = error.fatal,
            error = %error,
            source = %source,
            "client command failed"
        );
    } else {
        tracing::warn!(
            operation = error.operation,
            request_kind = %request_kind,
            request_stream = %request_stream,
            code = ?error.kind,
            fatal = error.fatal,
            error = %error,
            "client command failed"
        );
    }

    let payload = error.to_payload(request_stream, request_kind);
    let payload = match serde_json::to_value(&payload) {
        Ok(value) => value,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialize command error payload");
            return;
        }
    };
    let _ = host_output_stream.send_value(FrameKind::CommandError, payload);
}
