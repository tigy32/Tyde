use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use protocol::{
    FrameKind, ProtocolFrame, SendMessagePayload, StreamPath, VoiceAcceptedPayload,
    VoiceAudioFormat, VoiceAudioPayload, VoiceDirection, VoiceErrorCode, VoiceErrorPayload,
    VoiceFlowStats, VoiceFormatPair, VoiceSessionId, VoiceSessionPayload, VoiceSessionState,
    VoiceStartPayload, VoiceStatePayload, VoiceStopPayload, VoiceStopReason,
    VoiceTranscriptPayload,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::host::HostHandle;
use crate::stream::Stream;

const TOOL_INACTIVITY: Duration = Duration::from_secs(90);
const TOOL_TOTAL: Duration = Duration::from_secs(450);
const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;

struct ObserverGuard(crate::stream::OutputQueue);
impl Drop for ObserverGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn bounded_tool_result(mut value: String) -> String {
    if value.len() <= MAX_TOOL_RESULT_BYTES {
        return value;
    }
    let mut end = MAX_TOOL_RESULT_BYTES.saturating_sub("\n[truncated]".len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n[truncated]");
    value
}

struct ToolDeadline {
    started: Instant,
    last_event: Instant,
    last_progress: Instant,
}
impl ToolDeadline {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_event: now,
            last_progress: now - Duration::from_secs(2),
        }
    }
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.started) >= TOOL_TOTAL
            || now.duration_since(self.last_event) >= TOOL_INACTIVITY
    }
    fn observe(&mut self, now: Instant) -> bool {
        self.last_event = now;
        if now.duration_since(self.last_progress) >= Duration::from_secs(2) {
            self.last_progress = now;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NovaInput {
    Audio16Khz(Vec<i16>),
    Interrupt {
        output_generation: u64,
    },
    InputEnd,
    ToolResult {
        tool_use_id: String,
        message_id: String,
        result: String,
    },
    Progress {
        text: String,
    },
    Stop,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NovaSend {
    Sent,
    DroppedAudio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NovaOutput {
    Transcript {
        speaker: protocol::VoiceTranscriptSpeaker,
        text: String,
        is_final: bool,
    },
    Audio24Khz {
        output_generation: u64,
        samples: Vec<i16>,
    },
    State(VoiceSessionState),
    ToolUse {
        tool_use_id: String,
        model_turn_id: protocol::ModelTurnId,
        message: String,
    },
    ProviderError {
        code: VoiceErrorCode,
        retryable: bool,
        detail: Option<String>,
    },
    End {
        clean: bool,
    },
}

pub(crate) type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProviderFailure>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderFailure {
    pub code: VoiceErrorCode,
    pub retryable: bool,
    pub category: &'static str,
}

pub(crate) trait NovaSession: Send {
    fn send(&mut self, input: NovaInput) -> Result<NovaSend, ProviderFailure>;
    fn output(&mut self) -> &mut mpsc::Receiver<NovaOutput>;
    fn close(&mut self);
}

pub(crate) trait NovaProvider: Send + Sync {
    fn open<'a>(
        &'a self,
        settings: &'a settings_model::VoiceSettings,
        session: &'a VoiceSessionId,
    ) -> ProviderFuture<'a, Box<dyn NovaSession>>;
}

async fn open_provider(
    provider: std::sync::Arc<dyn NovaProvider>,
    settings: settings_model::VoiceSettings,
    session: VoiceSessionId,
) -> Result<Box<dyn NovaSession>, ProviderFailure> {
    provider.open(&settings, &session).await
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct SyntheticNovaProvider;

#[cfg(any(test, feature = "test-support"))]
impl NovaProvider for SyntheticNovaProvider {
    fn open<'a>(
        &'a self,
        _: &'a settings_model::VoiceSettings,
        _: &'a VoiceSessionId,
    ) -> ProviderFuture<'a, Box<dyn NovaSession>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let (input_tx, mut input_rx) = mpsc::channel(16);
            let (output_tx, output_rx) = mpsc::channel(32);
            tokio::spawn(async move {
                let mut started = false;
                let mut produced_output = false;
                let mut hands_free_interrupted = false;
                let mut output_generation = 0;
                while let Some(input) = input_rx.recv().await {
                    match input {
                        NovaInput::Audio16Khz(_) if !started => {
                            started = true;
                            let _ = output_tx
                                .send(NovaOutput::Transcript {
                                    speaker: protocol::VoiceTranscriptSpeaker::User,
                                    text: "synthetic request".into(),
                                    is_final: true,
                                })
                                .await;
                            let _ = output_tx
                                .send(NovaOutput::State(VoiceSessionState::AgentWorking))
                                .await;
                            let _ = output_tx
                                .send(NovaOutput::ToolUse {
                                    tool_use_id: "synthetic-tool".into(),
                                    model_turn_id: protocol::ModelTurnId("synthetic-turn".into()),
                                    message: "complete the synthetic voice task".into(),
                                })
                                .await;
                        }
                        NovaInput::Progress { text } => {
                            let _ = output_tx
                                .send(NovaOutput::Transcript {
                                    speaker: protocol::VoiceTranscriptSpeaker::Progress,
                                    text,
                                    is_final: false,
                                })
                                .await;
                        }
                        NovaInput::Interrupt {
                            output_generation: next,
                        } => {
                            let stale = output_generation;
                            output_generation = next;
                            if !hands_free_interrupted {
                                let _ = output_tx
                                    .send(NovaOutput::State(VoiceSessionState::Interrupting))
                                    .await;
                            }
                            let _ = output_tx
                                .send(NovaOutput::Audio24Khz {
                                    output_generation: stale,
                                    samples: vec![0; 480],
                                })
                                .await;
                        }
                        NovaInput::ToolResult { result, .. } => {
                            produced_output = true;
                            let _ = output_tx
                                .send(NovaOutput::Transcript {
                                    speaker: protocol::VoiceTranscriptSpeaker::Assistant,
                                    text: result,
                                    is_final: true,
                                })
                                .await;
                            let _ = output_tx
                                .send(NovaOutput::State(VoiceSessionState::Speaking))
                                .await;
                            let _ = output_tx
                                .send(NovaOutput::Audio24Khz {
                                    output_generation,
                                    samples: vec![0; 480],
                                })
                                .await;
                        }
                        NovaInput::Audio16Khz(_)
                            if started && produced_output && !hands_free_interrupted =>
                        {
                            hands_free_interrupted = true;
                            let _ = output_tx
                                .send(NovaOutput::State(VoiceSessionState::Interrupting))
                                .await;
                            // Per Nova's per-content event ordering, every
                            // audio frame of the interrupted content precedes
                            // the INTERRUPTED marker — so audio emitted after
                            // it models the NEXT response, still stamped with
                            // the current generation. It must reach playback;
                            // treating it as stale is the bug that muted every
                            // response after the first (beta.60 live QA).
                            let _ = output_tx
                                .send(NovaOutput::Audio24Khz {
                                    output_generation,
                                    samples: vec![0; 480],
                                })
                                .await;
                        }
                        NovaInput::Stop => {
                            let _ = output_tx.send(NovaOutput::End { clean: true }).await;
                            break;
                        }
                        NovaInput::Audio16Khz(_) | NovaInput::InputEnd => {}
                    }
                }
            });
            Ok(Box::new(SyntheticNovaSession {
                input_tx: Some(input_tx),
                output_rx,
            }) as Box<dyn NovaSession>)
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
struct SyntheticNovaSession {
    input_tx: Option<mpsc::Sender<NovaInput>>,
    output_rx: mpsc::Receiver<NovaOutput>,
}

#[cfg(any(test, feature = "test-support"))]
impl NovaSession for SyntheticNovaSession {
    fn send(&mut self, input: NovaInput) -> Result<NovaSend, ProviderFailure> {
        self.input_tx
            .as_ref()
            .ok_or(ProviderFailure {
                code: VoiceErrorCode::ProviderUnavailable,
                retryable: false,
                category: "synthetic_closed",
            })?
            .try_send(input)
            .map(|_| NovaSend::Sent)
            .map_err(|_| ProviderFailure {
                code: VoiceErrorCode::ProviderUnavailable,
                retryable: true,
                category: "synthetic_backpressure",
            })
    }
    fn output(&mut self) -> &mut mpsc::Receiver<NovaOutput> {
        &mut self.output_rx
    }
    fn close(&mut self) {
        self.input_tx.take();
    }
}

struct ActiveVoice {
    id: VoiceSessionId,
    generation: u64,
    target: protocol::VoiceTarget,
    decoder: opus::Decoder,
    encoder: opus::Encoder,
    provider: Box<dyn NovaSession>,
    next_input_seq: u64,
    next_output_seq: u64,
    output_timestamp_48k: u64,
    stats: VoiceFlowStats,
    cancel: CancellationToken,
    tool_tx: mpsc::Sender<NovaInput>,
    tool_rx: mpsc::Receiver<NovaInput>,
    output_active: bool,
    output_generation: u64,
    stale_output_chunks: u64,
    stale_output_samples: u64,
    started_at: Instant,
    last_target_check: Instant,
    queue_drop_baseline: (u64, u64),
    input_ended: bool,
}

struct PendingVoice {
    id: VoiceSessionId,
    generation: u64,
    target: protocol::VoiceTarget,
    pair: VoiceFormatPair,
    open: tokio::task::JoinHandle<Result<Box<dyn NovaSession>, ProviderFailure>>,
    last_target_check: Instant,
}

pub(crate) struct VoiceConnection {
    host: HostHandle,
    connection_host_stream: StreamPath,
    output: Stream,
    provider: std::sync::Arc<dyn NovaProvider>,
    pending: Option<PendingVoice>,
    active: Option<ActiveVoice>,
    last_generation: u64,
}

impl Drop for VoiceConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl VoiceConnection {
    pub fn new(
        host: HostHandle,
        output: Stream,
        provider: std::sync::Arc<dyn NovaProvider>,
    ) -> Self {
        let connection_host_stream = output.path().clone();
        Self {
            host,
            connection_host_stream,
            output,
            provider,
            pending: None,
            active: None,
            last_generation: 0,
        }
    }

    pub async fn handle(&mut self, frame: ProtocolFrame) -> Result<(), String> {
        if !frame.envelope.stream.0.starts_with("/voice") {
            return Err("voice frame on non-voice stream".into());
        }
        if frame.envelope.kind != FrameKind::VoiceStart
            && frame_generation(&frame).is_some_and(|generation| generation < self.last_generation)
        {
            return Ok(());
        }
        if frame.envelope.kind == FrameKind::VoiceStart {
            if frame.envelope.stream.0 != "/voice" {
                return Err("VoiceStart must use /voice".into());
            }
        } else if let Some(active) = &self.active {
            let expected = StreamPath(format!("/voice/{}", active.id.0));
            if frame.envelope.stream != expected {
                return Err("voice session stream mismatch".into());
            }
        }
        match frame.envelope.kind {
            FrameKind::VoiceStart => self.start(frame).await,
            FrameKind::VoiceAudio => self.audio(frame),
            FrameKind::VoiceInputEnd => self.input_end(frame),
            FrameKind::VoiceInterrupt => {
                self.interrupt(frame)?;
                self.apply_interrupt(false)
            }
            FrameKind::VoiceStop => self.stop(frame),
            _ => Err(format!(
                "unexpected client voice kind {}",
                frame.envelope.kind
            )),
        }
    }

    async fn start(&mut self, frame: ProtocolFrame) -> Result<(), String> {
        if !frame.binary.is_empty() {
            return Err("VoiceStart must not have a body".into());
        }
        let start: VoiceStartPayload = frame
            .envelope
            .parse_payload()
            .map_err(|_| "invalid VoiceStart")?;
        if self.active.is_some()
            || self.pending.is_some()
            || start.generation <= self.last_generation
        {
            return self.emit_error(
                None,
                start.generation,
                VoiceErrorCode::AlreadyActive,
                false,
                true,
            );
        }
        let Some(pair) = start
            .formats
            .iter()
            .find(|pair| {
                pair.uplink.valid()
                    && pair.downlink.valid()
                    && pair.uplink == VoiceAudioFormat::opus(48_000)
                    && pair.downlink == VoiceAudioFormat::opus(24_000)
            })
            .cloned()
        else {
            return self.emit_error(
                None,
                start.generation,
                VoiceErrorCode::InvalidRequest,
                false,
                true,
            );
        };
        if self
            .host
            .agent_handle_for_instance(&self.connection_host_stream, &start.target)
            .await
            .is_none()
        {
            return self.emit_error(
                None,
                start.generation,
                VoiceErrorCode::WrongTarget,
                false,
                true,
            );
        }
        let settings = self
            .host
            .read_settings()
            .await
            .map_err(|_| "settings unavailable")?;
        if !settings.voice.enabled {
            return self.emit_error(
                None,
                start.generation,
                VoiceErrorCode::NotAvailable,
                false,
                true,
            );
        }
        let id = VoiceSessionId(Uuid::new_v4().to_string());
        self.last_generation = start.generation;
        let provider = self.provider.clone();
        let voice_settings = settings.voice.clone();
        let open_id = id.clone();
        let open = tokio::spawn(open_provider(provider, voice_settings, open_id));
        self.pending = Some(PendingVoice {
            id,
            generation: start.generation,
            target: start.target,
            pair,
            open,
            last_target_check: Instant::now(),
        });
        Ok(())
    }

    fn audio(&mut self, frame: ProtocolFrame) -> Result<(), String> {
        let payload: VoiceAudioPayload = frame
            .envelope
            .parse_payload()
            .map_err(|_| "invalid VoiceAudio")?;
        payload
            .validate_body(frame.binary.len())
            .map_err(str::to_owned)?;
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| "no active voice session".to_owned())?;
        validate_session(active, &payload.session_id, payload.generation)?;
        if active.input_ended {
            return Err("voice input already ended".into());
        }
        if payload.direction != VoiceDirection::Input {
            return Err("client VoiceAudio must be input".into());
        }
        if payload.timestamp_samples_48k != payload.first_media_seq.saturating_mul(960) {
            return Err("voice input timestamp/sequence mismatch".into());
        }
        if payload.first_media_seq < active.next_input_seq {
            active.stats.dropped_packets = active
                .stats
                .dropped_packets
                .saturating_add(payload.packet_lengths.len() as u64);
            active.stats.dropped_bytes = active
                .stats
                .dropped_bytes
                .saturating_add(frame.binary.len() as u64);
            return Ok(());
        }
        let skipped = payload
            .first_media_seq
            .saturating_sub(active.next_input_seq);
        active.stats.dropped_packets = active.stats.dropped_packets.saturating_add(skipped);
        for _ in 0..skipped.min(8) {
            let mut plc = vec![0i16; 320];
            if let Ok(samples) = active.decoder.decode(&[], &mut plc, true) {
                plc.truncate(samples);
                let _ = active.provider.send(NovaInput::Audio16Khz(plc));
            }
        }
        let mut offset = 0;
        let mut admitted_packets = 0u64;
        let mut admitted_bytes = 0u64;
        for length in &payload.packet_lengths {
            let end = offset + usize::from(*length);
            let mut pcm = vec![0i16; 960];
            let decoded = active
                .decoder
                .decode(&frame.binary[offset..end], &mut pcm, false)
                .map_err(|_| "invalid Opus packet")?;
            pcm.truncate(decoded);
            match active
                .provider
                .send(NovaInput::Audio16Khz(pcm))
                .map_err(|_| "provider input closed")?
            {
                NovaSend::Sent => {
                    admitted_packets += 1;
                    admitted_bytes += *length as u64;
                }
                NovaSend::DroppedAudio => {
                    active.stats.dropped_packets = active.stats.dropped_packets.saturating_add(1);
                    active.stats.dropped_bytes =
                        active.stats.dropped_bytes.saturating_add(*length as u64);
                }
            }
            offset = end;
        }
        active.next_input_seq = payload.first_media_seq + payload.packet_lengths.len() as u64;
        active.stats.admitted_packets += admitted_packets;
        active.stats.admitted_bytes += admitted_bytes;
        Ok(())
    }

    fn interrupt(&mut self, frame: ProtocolFrame) -> Result<(), String> {
        if !frame.binary.is_empty() {
            return Err("VoiceInterrupt must not have a binary body".into());
        }
        let payload: VoiceSessionPayload = frame
            .envelope
            .parse_payload()
            .map_err(|_| "invalid VoiceInterrupt")?;
        let active = self.active.as_ref().ok_or("no active voice session")?;
        validate_session(active, &payload.session_id, payload.generation)
    }

    fn apply_interrupt(&mut self, provider_reported: bool) -> Result<(), String> {
        let active = self.active.as_mut().ok_or("no active voice session")?;
        // A provider-reported interrupt is already ordered in the provider's
        // event lane: every audio frame of the interrupted content precedes
        // its INTERRUPTED marker, so nothing future remains to stale-drop —
        // only the locally queued tail, which the flush below discards.
        // Minting a generation here races the provider's per-completion
        // stamp (snapshotted at completionStart, possibly parsed before this
        // runs) and leaves the expected generation permanently one ahead:
        // every response after the first is then silently muted. Only
        // client-initiated interrupts bump, because there the provider keeps
        // streaming the interrupted content and the stale stamp is what
        // discards its remainder; that bump and the provider's store happen
        // synchronously on this task, so no snapshot can slip between them.
        if !provider_reported {
            active.output_generation = active.output_generation.wrapping_add(1);
            active
                .provider
                .send(NovaInput::Interrupt {
                    output_generation: active.output_generation,
                })
                .map_err(|_| "provider interrupt failed")?;
        }
        let (packets, bytes) = self
            .output
            .discard_voice_audio(&active.id, active.generation);
        active.stats.dropped_packets += packets;
        active.stats.dropped_bytes += bytes;
        active.output_active = false;
        let payload = VoiceStatePayload {
            session_id: active.id.clone(),
            generation: active.generation,
            state: VoiceSessionState::Interrupting,
            model_turn_id: None,
        };
        self.output
            .with_path(StreamPath(format!("/voice/{}", active.id.0)))
            .send_value(
                FrameKind::VoiceState,
                serde_json::to_value(payload).unwrap_or_default(),
            )
            .map_err(|_| "output closed".into())
    }

    fn input_end(&mut self, frame: ProtocolFrame) -> Result<(), String> {
        if !frame.binary.is_empty() {
            return Err("VoiceInputEnd must not have a binary body".into());
        }
        let payload: VoiceSessionPayload = frame
            .envelope
            .parse_payload()
            .map_err(|_| "invalid VoiceInputEnd")?;
        let active = self.active.as_mut().ok_or("no active voice session")?;
        validate_session(active, &payload.session_id, payload.generation)?;
        if active.input_ended {
            return Err("duplicate VoiceInputEnd".into());
        }
        active.input_ended = true;
        active
            .provider
            .send(NovaInput::InputEnd)
            .map(|_| ())
            .map_err(|_| "provider input closed".into())
    }

    fn stop(&mut self, frame: ProtocolFrame) -> Result<(), String> {
        if !frame.binary.is_empty() {
            return Err("VoiceStop must not have a binary body".into());
        }
        let payload = frame
            .envelope
            .parse_payload::<VoiceStopPayload>()
            .map_err(|_| "invalid VoiceStop")?;
        let active = self.active.as_mut().ok_or("no active voice session")?;
        validate_session(active, &payload.session_id, payload.generation)?;
        active.stats.dropped_packets = active
            .stats
            .dropped_packets
            .saturating_add(payload.stats.dropped_packets);
        active.stats.dropped_bytes = active
            .stats
            .dropped_bytes
            .saturating_add(payload.stats.dropped_bytes);
        active.stats.queue_high_water_packets = active
            .stats
            .queue_high_water_packets
            .max(payload.stats.queue_high_water_packets);
        self.finish(payload.reason);
        Ok(())
    }

    fn finish(&mut self, reason: VoiceStopReason) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let stream = StreamPath(format!("/voice/{}", active.id.0));
        let ending = VoiceStatePayload {
            session_id: active.id.clone(),
            generation: active.generation,
            state: VoiceSessionState::Ending,
            model_turn_id: None,
        };
        let _ = self.output.with_path(stream.clone()).send_value(
            FrameKind::VoiceState,
            serde_json::to_value(ending).unwrap_or_default(),
        );
        active.cancel.cancel();
        // Nova requires every open content block to be closed before promptEnd;
        // ending the prompt with the microphone content still open draws a
        // ValidationException ("All contents must be closed before ending
        // prompt") on every teardown.
        if !active.input_ended {
            let _ = active.provider.send(NovaInput::InputEnd);
        }
        let _ = active.provider.send(NovaInput::Stop);
        active.provider.close();
        let (packets, bytes) = self
            .output
            .discard_voice_audio(&active.id, active.generation);
        active.stats.dropped_packets += packets;
        active.stats.dropped_bytes += bytes;
        let queue_metrics = self.output.audio_metrics();
        active.stats.dropped_packets +=
            queue_metrics.0.saturating_sub(active.queue_drop_baseline.0);
        active.stats.dropped_bytes += queue_metrics.1.saturating_sub(active.queue_drop_baseline.1);
        active.stats.queue_high_water_packets = active
            .stats
            .queue_high_water_packets
            .max(queue_metrics.2.min(8));
        // The flow counters are the only quantitative record of audio health
        // (drops, gaps, queue pressure); without this line they die with the
        // session and "choppy audio" reports are unfalsifiable from host logs.
        tracing::info!(
            session = %active.id.0,
            reason = ?reason,
            admitted_packets = active.stats.admitted_packets,
            admitted_bytes = active.stats.admitted_bytes,
            dropped_packets = active.stats.dropped_packets,
            dropped_bytes = active.stats.dropped_bytes,
            queue_high_water_packets = active.stats.queue_high_water_packets,
            stale_output_chunks = active.stale_output_chunks,
            stale_output_samples = active.stale_output_samples,
            "voice session ended"
        );
        let ended = VoiceStatePayload {
            session_id: active.id.clone(),
            generation: active.generation,
            state: VoiceSessionState::Ended,
            model_turn_id: None,
        };
        let _ = self.output.with_path(stream.clone()).send_value(
            FrameKind::VoiceState,
            serde_json::to_value(ended).unwrap_or_default(),
        );
        let payload = VoiceStopPayload {
            session_id: active.id,
            generation: active.generation,
            reason,
            stats: active.stats,
        };
        let _ = self.output.with_path(stream).send_value(
            FrameKind::VoiceStop,
            serde_json::to_value(payload).unwrap_or_default(),
        );
    }

    fn emit_state(&self, active: &ActiveVoice, state: VoiceSessionState) {
        let payload = VoiceStatePayload {
            session_id: active.id.clone(),
            generation: active.generation,
            state,
            model_turn_id: None,
        };
        let _ = self
            .output
            .with_path(StreamPath(format!("/voice/{}", active.id.0)))
            .send_value(
                FrameKind::VoiceState,
                serde_json::to_value(payload).unwrap_or_default(),
            );
    }

    fn emit_error(
        &self,
        id: Option<VoiceSessionId>,
        generation: u64,
        code: VoiceErrorCode,
        retryable: bool,
        fatal: bool,
    ) -> Result<(), String> {
        self.emit_error_with_detail(id, generation, code, retryable, fatal, None)
    }

    fn emit_error_with_detail(
        &self,
        id: Option<VoiceSessionId>,
        generation: u64,
        code: VoiceErrorCode,
        retryable: bool,
        fatal: bool,
        detail: Option<String>,
    ) -> Result<(), String> {
        let path = id.as_ref().map_or_else(
            || StreamPath("/voice".into()),
            |session| StreamPath(format!("/voice/{}", session.0)),
        );
        let payload = VoiceErrorPayload {
            session_id: id,
            generation,
            code,
            retryable,
            fatal,
            detail,
        };
        self.output
            .with_path(path)
            .send_value(
                FrameKind::VoiceError,
                serde_json::to_value(payload).map_err(|_| "encode error")?,
            )
            .map_err(|_| "output closed".into())
    }

    pub fn shutdown(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.open.abort();
        }
        self.finish(VoiceStopReason::TransportLost);
    }
    pub fn fail_invalid_frame(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.open.abort();
        }
        let identity = self
            .active
            .as_ref()
            .map(|active| (active.id.clone(), active.generation));
        if let Some((id, generation)) = identity {
            let _ = self.emit_error(
                Some(id),
                generation,
                VoiceErrorCode::InvalidAudio,
                false,
                true,
            );
        }
        self.finish(VoiceStopReason::MediaFailed);
    }

    pub async fn drain_output(&mut self) -> Result<(), String> {
        if self.pending.as_ref().is_some_and(|pending| {
            pending.last_target_check.elapsed() >= Duration::from_millis(250)
        }) {
            let target = self.pending.as_ref().expect("pending voice").target.clone();
            if self
                .host
                .agent_handle_for_instance(&self.connection_host_stream, &target)
                .await
                .is_none()
            {
                let pending = self.pending.take().expect("pending voice");
                pending.open.abort();
                self.emit_error(
                    None,
                    pending.generation,
                    VoiceErrorCode::WrongTarget,
                    false,
                    true,
                )?;
                return Ok(());
            }
            if let Some(pending) = self.pending.as_mut() {
                pending.last_target_check = Instant::now();
            }
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.open.is_finished())
        {
            let pending = self.pending.take().expect("finished pending voice");
            match pending.open.await.map_err(|_| "Nova startup task failed")? {
                Ok(mut provider) => {
                    if self
                        .host
                        .agent_handle_for_instance(&self.connection_host_stream, &pending.target)
                        .await
                        .is_none()
                    {
                        provider.close();
                        self.emit_error(
                            None,
                            pending.generation,
                            VoiceErrorCode::WrongTarget,
                            false,
                            true,
                        )?;
                        return Ok(());
                    }
                    let decoder = opus::Decoder::new(16_000, opus::Channels::Mono)
                        .map_err(|_| "Opus decoder unavailable")?;
                    let encoder =
                        opus::Encoder::new(24_000, opus::Channels::Mono, opus::Application::Voip)
                            .map_err(|_| "Opus encoder unavailable")?;
                    let (tool_tx, tool_rx) = mpsc::channel(16);
                    let queue_metrics = self.output.audio_metrics();
                    self.active = Some(ActiveVoice {
                        id: pending.id.clone(),
                        generation: pending.generation,
                        target: pending.target.clone(),
                        decoder,
                        encoder,
                        provider,
                        next_input_seq: 0,
                        next_output_seq: 0,
                        output_timestamp_48k: 0,
                        stats: VoiceFlowStats::default(),
                        cancel: CancellationToken::new(),
                        tool_tx,
                        tool_rx,
                        output_active: false,
                        output_generation: 0,
                        stale_output_chunks: 0,
                        stale_output_samples: 0,
                        started_at: Instant::now(),
                        last_target_check: Instant::now(),
                        queue_drop_baseline: (queue_metrics.0, queue_metrics.1),
                        input_ended: false,
                    });
                    let accepted = VoiceAcceptedPayload {
                        session_id: pending.id,
                        generation: pending.generation,
                        target: pending.target,
                        uplink: pending.pair.uplink,
                        downlink: pending.pair.downlink,
                    };
                    self.output
                        .with_path(StreamPath("/voice".into()))
                        .send_value(
                            FrameKind::VoiceAccepted,
                            serde_json::to_value(accepted).map_err(|_| "encode accepted")?,
                        )
                        .map_err(|_| "output closed")?;
                    if let Some(active) = &self.active {
                        self.emit_state(active, VoiceSessionState::Listening);
                    }
                }
                Err(failure) => {
                    tracing::warn!(
                        category = failure.category,
                        "Nova voice session was not accepted"
                    );
                    self.emit_error(
                        None,
                        pending.generation,
                        failure.code,
                        failure.retryable,
                        true,
                    )?;
                }
            }
        }
        if self.active.as_ref().is_some_and(|active| {
            active.started_at.elapsed() >= Duration::from_secs(protocol::VOICE_SESSION_MAX_SECONDS)
        }) {
            self.finish(VoiceStopReason::Inactivity);
            return Ok(());
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.last_target_check.elapsed() >= Duration::from_millis(250))
        {
            let target = self.active.as_ref().unwrap().target.clone();
            let still_owned = self
                .host
                .agent_handle_for_instance(&self.connection_host_stream, &target)
                .await
                .is_some();
            if !still_owned {
                self.finish(VoiceStopReason::AgentClosed);
                return Ok(());
            }
            if let Some(active) = self.active.as_mut() {
                active.last_target_check = Instant::now();
            }
        }
        let mut provider_completed = false;
        let mut provider_ended = false;
        let mut provider_error = None;
        let mut provider_interrupt = false;
        {
            let Some(active) = self.active.as_mut() else {
                return Ok(());
            };
            while let Ok(input) = active.tool_rx.try_recv() {
                // Progress is a UI-only signal (the provider prompt must not
                // receive it — Nova allows one SYSTEM block per prompt), so
                // broadcast it as a transcript here, provider-independently.
                if let NovaInput::Progress { text } = &input {
                    let payload = VoiceTranscriptPayload {
                        session_id: active.id.clone(),
                        generation: active.generation,
                        speaker: protocol::VoiceTranscriptSpeaker::Progress,
                        text: text.clone(),
                        is_final: false,
                        message_id: None,
                    };
                    self.output
                        .with_path(StreamPath(format!("/voice/{}", active.id.0)))
                        .send_value(
                            FrameKind::VoiceTranscript,
                            serde_json::to_value(payload)
                                .map_err(|_| "encode progress transcript")?,
                        )
                        .map_err(|_| "output closed")?;
                }
                active
                    .provider
                    .send(input)
                    .map_err(|_| "provider tool channel closed")?;
            }
            while let Ok(output) = active.provider.output().try_recv() {
                match output {
                    NovaOutput::Transcript {
                        speaker,
                        text,
                        is_final,
                    } => {
                        let payload = VoiceTranscriptPayload {
                            session_id: active.id.clone(),
                            generation: active.generation,
                            speaker,
                            text,
                            is_final,
                            message_id: None,
                        };
                        self.output
                            .with_path(StreamPath(format!("/voice/{}", active.id.0)))
                            .send_value(
                                FrameKind::VoiceTranscript,
                                serde_json::to_value(payload).map_err(|_| "encode transcript")?,
                            )
                            .map_err(|_| "output closed")?;
                    }
                    NovaOutput::Audio24Khz {
                        output_generation,
                        samples,
                    } => {
                        if output_generation != active.output_generation {
                            // Stale tail of a client-interrupted response.
                            // Count it: an uncounted `continue` here is how
                            // "only the first response is audible" hid from
                            // the flow stats for three betas.
                            active.stale_output_chunks += 1;
                            active.stale_output_samples += samples.len() as u64;
                            tracing::debug!(
                                session = %active.id.0,
                                output_generation,
                                expected = active.output_generation,
                                samples = samples.len(),
                                "discarded stale voice output"
                            );
                            continue;
                        }
                        if !active.output_active {
                            active.output_active = true;
                            let state = VoiceStatePayload {
                                session_id: active.id.clone(),
                                generation: active.generation,
                                state: VoiceSessionState::Speaking,
                                model_turn_id: None,
                            };
                            self.output
                                .with_path(StreamPath(format!("/voice/{}", active.id.0)))
                                .send_value(
                                    FrameKind::VoiceState,
                                    serde_json::to_value(state).unwrap_or_default(),
                                )
                                .map_err(|_| "output closed")?;
                            let payload = VoiceSessionPayload {
                                session_id: active.id.clone(),
                                generation: active.generation,
                            };
                            self.output
                                .with_path(StreamPath(format!("/voice/{}", active.id.0)))
                                .send_value(
                                    FrameKind::VoiceOutput,
                                    serde_json::to_value(payload).unwrap_or_default(),
                                )
                                .map_err(|_| "output closed")?;
                        }
                        for pcm in samples.chunks(480) {
                            if pcm.len() != 480 {
                                break;
                            }
                            let mut packet = vec![0; 1275];
                            let len = active
                                .encoder
                                .encode(pcm, &mut packet)
                                .map_err(|_| "Opus output encode failed")?;
                            packet.truncate(len);
                            let payload = VoiceAudioPayload {
                                session_id: active.id.clone(),
                                generation: active.generation,
                                direction: VoiceDirection::Output,
                                first_media_seq: active.next_output_seq,
                                timestamp_samples_48k: active.output_timestamp_48k,
                                packet_lengths: vec![len as u16],
                            };
                            // Downlink audio rides its own sub-stream with its
                            // own sequence counter. The shell consumes these
                            // frames natively, so if they shared the JSON
                            // envelope stream's counter the frontend validator
                            // (a partial observer) would desync at Nova's
                            // first spoken word and gray out the connection.
                            self.output
                                .with_path(StreamPath(format!("/voice/{}/audio", active.id.0)))
                                .send_binary(
                                    FrameKind::VoiceAudio,
                                    serde_json::to_value(payload).unwrap_or_default(),
                                    packet,
                                )
                                .map_err(|_| "output closed")?;
                            active.next_output_seq += 1;
                            active.output_timestamp_48k += 960;
                            active.stats.admitted_packets += 1;
                            active.stats.admitted_bytes += len as u64;
                        }
                    }
                    NovaOutput::State(VoiceSessionState::Interrupting) => {
                        provider_interrupt = true;
                        break;
                    }
                    NovaOutput::State(state) => {
                        let payload = VoiceStatePayload {
                            session_id: active.id.clone(),
                            generation: active.generation,
                            state,
                            model_turn_id: None,
                        };
                        self.output
                            .with_path(StreamPath(format!("/voice/{}", active.id.0)))
                            .send_value(
                                FrameKind::VoiceState,
                                serde_json::to_value(payload).unwrap_or_default(),
                            )
                            .map_err(|_| "output closed")?;
                    }
                    NovaOutput::ToolUse {
                        tool_use_id,
                        model_turn_id,
                        message,
                    } => {
                        let state = VoiceStatePayload {
                            session_id: active.id.clone(),
                            generation: active.generation,
                            state: VoiceSessionState::AgentWorking,
                            model_turn_id: Some(model_turn_id),
                        };
                        self.output
                            .with_path(StreamPath(format!("/voice/{}", active.id.0)))
                            .send_value(
                                FrameKind::VoiceState,
                                serde_json::to_value(state)
                                    .map_err(|_| "encode agent-working state")?,
                            )
                            .map_err(|_| "output closed")?;
                        let host = self.host.clone();
                        let connection_host_stream = self.connection_host_stream.clone();
                        let target = active.target.clone();
                        let tx = active.tool_tx.clone();
                        let cancel = active.cancel.clone();
                        tokio::spawn(async move {
                            let failure_tx = tx.clone();
                            let failed_tool_id = tool_use_id.clone();
                            let result = run_agent_tool(
                                host,
                                connection_host_stream,
                                target,
                                message,
                                tx,
                                tool_use_id,
                                cancel.clone(),
                            )
                            .await;
                            if result.is_err() && !cancel.is_cancelled() {
                                let _ = failure_tx
                                    .send(NovaInput::ToolResult {
                                        tool_use_id: failed_tool_id,
                                        message_id: "tool-delivery-failed".into(),
                                        result: "The focused Tyde agent could not complete this tool request."
                                            .into(),
                                    })
                                    .await;
                            }
                        });
                    }
                    NovaOutput::End { clean } => {
                        if clean {
                            provider_completed = true;
                        } else {
                            provider_ended = true;
                        }
                        break;
                    }
                    NovaOutput::ProviderError {
                        code,
                        retryable,
                        detail,
                    } => {
                        provider_error = Some((
                            active.id.clone(),
                            active.generation,
                            code,
                            retryable,
                            detail,
                        ));
                        provider_ended = true;
                        break;
                    }
                }
            }
        }
        if provider_interrupt {
            self.apply_interrupt(true)?;
        }
        if provider_completed {
            self.finish(VoiceStopReason::ProviderCompleted);
            return Ok(());
        }
        if let Some((id, generation, code, retryable, detail)) = provider_error {
            tracing::warn!(
                session = %id.0,
                code = ?code,
                retryable,
                detail = detail.as_deref().unwrap_or(""),
                "voice provider error ended the session"
            );
            self.emit_error_with_detail(Some(id), generation, code, retryable, true, detail)?;
        }
        if provider_ended {
            self.finish(VoiceStopReason::ProviderFailed);
        }
        Ok(())
    }
}

fn validate_session(
    active: &ActiveVoice,
    id: &VoiceSessionId,
    generation: u64,
) -> Result<(), String> {
    if &active.id != id || active.generation != generation {
        Err("stale voice generation/session".into())
    } else {
        Ok(())
    }
}

fn frame_generation(frame: &ProtocolFrame) -> Option<u64> {
    match frame.envelope.kind {
        FrameKind::VoiceAudio => frame
            .envelope
            .parse_payload::<VoiceAudioPayload>()
            .ok()
            .map(|payload| payload.generation),
        FrameKind::VoiceStop => frame
            .envelope
            .parse_payload::<VoiceStopPayload>()
            .ok()
            .map(|payload| payload.generation),
        FrameKind::VoiceInputEnd | FrameKind::VoiceInterrupt => frame
            .envelope
            .parse_payload::<VoiceSessionPayload>()
            .ok()
            .map(|payload| payload.generation),
        _ => None,
    }
}

pub(crate) async fn run_agent_tool(
    host: HostHandle,
    connection_host_stream: StreamPath,
    target: protocol::VoiceTarget,
    message: String,
    progress: mpsc::Sender<NovaInput>,
    tool_use_id: String,
    cancel: CancellationToken,
) -> Result<(), String> {
    let agent = host
        .agent_handle_for_instance(&connection_host_stream, &target)
        .await
        .ok_or("agent target changed")?;
    let (observer_queue, mut observer_rx) = crate::stream::output_channel();
    if !agent
        .attach(Stream::new(
            target.instance_stream.clone(),
            observer_queue.clone(),
        ))
        .await
    {
        return Err("agent observer failed".into());
    }
    let _observer_guard = ObserverGuard(observer_queue.clone());
    agent
        .deliver_message(SendMessagePayload {
            message,
            images: None,
            origin: Some(protocol::MessageOrigin::User),
            tool_response: None,
        })
        .await?;
    let mut deadline = ToolDeadline::new(Instant::now());
    let mut message_id: Option<String> = None;
    loop {
        let now = Instant::now();
        if deadline.expired(now) {
            return Err("agent tool inactivity deadline".into());
        }
        let remaining = TOOL_INACTIVITY
            .saturating_sub(now.duration_since(deadline.last_event))
            .min(TOOL_TOTAL.saturating_sub(now.duration_since(deadline.started)));
        let item = tokio::select! {
            _ = cancel.cancelled() => return Err("voice tool observer cancelled".into()),
            result = tokio::time::timeout(remaining, observer_rx.recv()) => result.map_err(|_| "agent tool inactivity deadline")?.ok_or("agent observer closed")?,
        };
        if deadline.observe(Instant::now()) {
            let _ = progress.try_send(NovaInput::Progress {
                text: "The Tyde agent is still working.".into(),
            });
        }
        if item.kind != FrameKind::ChatEvent {
            continue;
        }
        let event: protocol::ChatEvent = item.parse_payload().map_err(|_| "invalid agent event")?;
        match event {
            protocol::ChatEvent::StreamStart(start) if message_id.is_none() => {
                message_id = start.message_id
            }
            protocol::ChatEvent::StreamEnd(end)
                if end.message.message_id.as_ref().map(|v| v.0.as_str())
                    == message_id.as_deref() =>
            {
                let id = message_id.ok_or("assistant stream missing message_id")?;
                tokio::select! {
                    _ = cancel.cancelled() => return Err("voice tool observer cancelled".into()),
                    sent = progress.send(NovaInput::ToolResult { tool_use_id, message_id: id, result: bounded_tool_result(end.message.content) }) => sent.map_err(|_|"provider tool channel closed")?,
                }
                return Ok(());
            }
            _ => {}
        }
    }
}
