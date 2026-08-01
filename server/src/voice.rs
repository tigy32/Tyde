use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use protocol::{
    AgentBootstrapPayload, ChatEvent, FrameKind, MAX_VOICE_ICE_CANDIDATES, MAX_VOICE_SDP_BYTES,
    MAX_VOICE_TOOL_MESSAGE_BYTES, MessageOrigin, SendMessagePayload, StreamPath,
    VOICE_SESSION_MAX_SECONDS, VoiceAgentProgress, VoiceAgentProgressKind, VoiceAnswerPayload,
    VoiceErrorCode, VoiceErrorPayload, VoiceIceCandidate, VoiceReadyPayload, VoiceSessionId,
    VoiceSessionState, VoiceSettings, VoiceStatePayload, VoiceStopReason, VoiceTarget,
    VoiceTranscript, VoiceTranscriptSpeaker,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::AgentHandle;
use crate::stream::Stream;

const AGENT_TOOL_NAME: &str = "send_to_focused_tyde_agent";
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);
const PROVIDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const PENDING_TOOL_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_PCM_SAMPLES_PER_FRAME: usize = 4_800;
const VOICE_COMMAND_QUEUE_CAPACITY: usize = 128;
pub(crate) const NOVA_OUTPUT_QUEUE_CAPACITY: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoicePcmFrame {
    pub sample_rate_hertz: u32,
    pub samples: Vec<i16>,
}

fn valid_pcm_frame(frame: &VoicePcmFrame) -> bool {
    matches!(frame.sample_rate_hertz, 8_000 | 16_000 | 24_000 | 48_000)
        && !frame.samples.is_empty()
        && frame.samples.len() <= MAX_PCM_SAMPLES_PER_FRAME
}

fn valid_audio_sdp(sdp: &str) -> bool {
    !sdp.is_empty()
        && sdp.len() <= MAX_VOICE_SDP_BYTES
        && sdp.contains("m=audio")
        && !sdp.contains("m=application")
        && !sdp.contains("m=video")
        && !sdp.contains('\0')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceRuntimeError {
    Unavailable,
    InvalidSignal,
    Closed,
    Provider(ProviderFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderFailure {
    pub code: VoiceErrorCode,
    pub category: String,
}

pub(crate) type VoiceMediaFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, VoiceRuntimeError>> + Send + 'a>>;

pub(crate) trait VoiceMediaSession: Send {
    fn accept_offer<'a>(&'a mut self, offer: &'a str) -> VoiceMediaFuture<'a, String>;
    fn add_ice_candidate<'a>(
        &'a mut self,
        candidate: &'a VoiceIceCandidate,
    ) -> VoiceMediaFuture<'a, ()>;
    fn end_ice_candidates(&mut self) -> VoiceMediaFuture<'_, ()>;
    fn take_input_audio(&mut self) -> Option<mpsc::Receiver<VoicePcmFrame>>;
    fn take_events(&mut self) -> Option<mpsc::Receiver<VoiceMediaEvent>>;
    fn play_output_audio(&mut self, frame: VoicePcmFrame) -> Result<(), VoiceRuntimeError>;
    fn close(&mut self);
}

#[derive(Debug)]
pub(crate) enum VoiceMediaEvent {
    Connected,
    Failed,
}

pub(crate) trait VoiceMediaFactory: Send + Sync {
    fn open(&self) -> VoiceMediaFuture<'_, Box<dyn VoiceMediaSession>>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NovaAudioOutputConfiguration {
    pub media_type: String,
    pub sample_rate_hertz: u32,
    pub sample_size_bits: u8,
    pub channel_count: u8,
    pub voice_id: String,
    pub encoding: String,
    pub audio_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NovaToolSpecification {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NovaInputEvent {
    event: NovaInputEventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum NovaInputEventKind {
    #[serde(rename = "sessionStart")]
    SessionStart {
        #[serde(rename = "inferenceConfiguration")]
        inference_configuration: serde_json::Value,
        #[serde(rename = "turnDetectionConfiguration")]
        turn_detection_configuration: serde_json::Value,
    },
    #[serde(rename = "promptStart")]
    PromptStart {
        #[serde(rename = "promptName")]
        prompt_name: String,
        #[serde(rename = "audioOutputConfiguration")]
        audio_output_configuration: NovaAudioOutputConfiguration,
        #[serde(rename = "textOutputConfiguration")]
        text_output_configuration: serde_json::Value,
        #[serde(rename = "toolUseOutputConfiguration")]
        tool_use_output_configuration: serde_json::Value,
        #[serde(rename = "toolConfiguration")]
        tool_configuration: serde_json::Value,
    },
    #[serde(rename = "contentStart")]
    ContentStart {
        #[serde(rename = "promptName")]
        prompt_name: String,
        #[serde(rename = "contentName")]
        content_name: String,
        role: String,
        #[serde(rename = "type")]
        content_type: String,
        interactive: bool,
        #[serde(flatten)]
        configuration: serde_json::Map<String, serde_json::Value>,
    },
    #[serde(rename = "textInput")]
    TextInput {
        #[serde(rename = "promptName")]
        prompt_name: String,
        #[serde(rename = "contentName")]
        content_name: String,
        content: String,
    },
    #[serde(rename = "audioInput")]
    AudioInput {
        #[serde(rename = "promptName")]
        prompt_name: String,
        #[serde(rename = "contentName")]
        content_name: String,
        content: String,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "promptName")]
        prompt_name: String,
        #[serde(rename = "contentName")]
        content_name: String,
        content: String,
    },
    #[serde(rename = "contentEnd")]
    ContentEnd {
        #[serde(rename = "promptName")]
        prompt_name: String,
        #[serde(rename = "contentName")]
        content_name: String,
    },
    #[serde(rename = "promptEnd")]
    PromptEnd {
        #[serde(rename = "promptName")]
        prompt_name: String,
    },
    #[serde(rename = "sessionEnd")]
    SessionEnd {},
}

impl NovaInputEvent {
    fn new(event: NovaInputEventKind) -> Self {
        Self { event }
    }
}

pub(crate) fn valid_external_nova_event(event: &NovaInputEvent) -> bool {
    let Ok(value) = serde_json::to_value(event) else {
        return false;
    };
    let Some(outer) = value.as_object() else {
        return false;
    };
    let Some(events) = outer.get("event").and_then(serde_json::Value::as_object) else {
        return false;
    };
    if outer.len() != 1 || events.len() != 1 {
        return false;
    }
    let Some((name, payload)) = events.iter().next() else {
        return false;
    };
    let Some(payload) = payload.as_object() else {
        return false;
    };
    let uuid_field = |field: &str| {
        payload
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
    };
    let nested = |path: &[&str]| {
        let (first, rest) = path.split_first()?;
        let mut value = payload.get(*first)?;
        for field in rest {
            value = match value {
                serde_json::Value::Object(object) => object.get(*field)?,
                serde_json::Value::Array(array) => array.get(field.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(value)
    };
    match name.as_str() {
        "sessionStart" => {
            payload
                .get("inferenceConfiguration")
                .is_some_and(serde_json::Value::is_object)
                && nested(&["turnDetectionConfiguration", "endpointingSensitivity"])
                    .and_then(serde_json::Value::as_str)
                    == Some("MEDIUM")
        }
        "promptStart" => {
            uuid_field("promptName")
                && nested(&["audioOutputConfiguration", "encoding"])
                    .and_then(serde_json::Value::as_str)
                    == Some("base64")
                && nested(&[
                    "toolConfiguration",
                    "tools",
                    "0",
                    "toolSpec",
                    "inputSchema",
                    "json",
                ])
                .is_some_and(serde_json::Value::is_object)
        }
        "contentStart" => {
            uuid_field("promptName")
                && uuid_field("contentName")
                && payload
                    .get("type")
                    .is_some_and(serde_json::Value::is_string)
                && payload
                    .get("role")
                    .is_some_and(serde_json::Value::is_string)
        }
        "textInput" => {
            uuid_field("promptName")
                && uuid_field("contentName")
                && payload
                    .get("content")
                    .is_some_and(serde_json::Value::is_string)
        }
        "audioInput" => {
            uuid_field("promptName")
                && uuid_field("contentName")
                && payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|content| {
                        base64::engine::general_purpose::STANDARD
                            .decode(content)
                            .is_ok()
                    })
        }
        "toolResult" => {
            uuid_field("promptName")
                && uuid_field("contentName")
                && payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|content| {
                        serde_json::from_str::<serde_json::Value>(content).is_ok()
                    })
        }
        "contentEnd" => uuid_field("promptName") && uuid_field("contentName"),
        "promptEnd" => uuid_field("promptName"),
        "sessionEnd" => payload.is_empty(),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum NovaOutputEvent {
    #[serde(rename = "toolUse")]
    ToolUse {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "interrupted")]
    Interrupted {},
    Transcript {
        speaker: VoiceTranscriptSpeaker,
        text: String,
        is_final: bool,
    },
    #[serde(skip)]
    AudioOutput(VoicePcmFrame),
    #[serde(skip)]
    ProviderFailed(ProviderFailure),
}

pub(crate) trait NovaSession: Send {
    fn send(&mut self, event: NovaInputEvent) -> Result<(), VoiceRuntimeError>;
    fn ready(&mut self) -> NovaReadyFuture<'_>;
    fn output(&mut self) -> &mut mpsc::Receiver<NovaOutputEvent>;
    fn close(&mut self);
}

pub(crate) type NovaOpenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn NovaSession>, VoiceRuntimeError>> + Send + 'a>>;
pub(crate) type NovaReadyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), VoiceRuntimeError>> + Send + 'a>>;

pub(crate) trait NovaProvider: Send + Sync {
    fn open<'a>(&'a self, settings: &'a VoiceSettings) -> NovaOpenFuture<'a>;
}

#[derive(Clone)]
pub(crate) struct VoiceRuntime {
    pub provider: Arc<dyn NovaProvider>,
    pub media: Arc<dyn VoiceMediaFactory>,
    available: bool,
}

impl VoiceRuntime {
    pub(crate) fn production() -> Self {
        Self {
            provider: Arc::new(crate::voice_aws::AwsNovaProvider),
            media: Arc::new(crate::voice_webrtc::Str0mMediaFactory),
            available: true,
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.available
    }

    pub(crate) fn unavailable() -> Self {
        Self {
            provider: Arc::new(UnavailableNovaProvider),
            media: Arc::new(UnavailableMediaFactory),
            available: false,
        }
    }

    pub(crate) fn mock() -> Self {
        Self {
            provider: Arc::new(MockNovaProvider),
            media: Arc::new(FakeMediaFactory),
            available: true,
        }
    }
}

struct UnavailableNovaProvider;

impl NovaProvider for UnavailableNovaProvider {
    fn open<'a>(&'a self, _settings: &'a VoiceSettings) -> NovaOpenFuture<'a> {
        Box::pin(async { Err(VoiceRuntimeError::Unavailable) })
    }
}

struct UnavailableMediaFactory;

impl VoiceMediaFactory for UnavailableMediaFactory {
    fn open(&self) -> VoiceMediaFuture<'_, Box<dyn VoiceMediaSession>> {
        Box::pin(async { Err(VoiceRuntimeError::Unavailable) })
    }
}

struct MockNovaProvider;

impl NovaProvider for MockNovaProvider {
    fn open<'a>(&'a self, _settings: &'a VoiceSettings) -> NovaOpenFuture<'a> {
        Box::pin(async { Ok(Box::new(MockNovaSession::standalone()) as Box<dyn NovaSession>) })
    }
}

pub(crate) struct MockNovaSession {
    sent: Arc<std::sync::Mutex<Vec<NovaInputEvent>>>,
    _output_tx: mpsc::Sender<NovaOutputEvent>,
    output_rx: mpsc::Receiver<NovaOutputEvent>,
}

impl MockNovaSession {
    fn standalone() -> Self {
        let (output_tx, output_rx) = mpsc::channel(NOVA_OUTPUT_QUEUE_CAPACITY);
        Self {
            sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            _output_tx: output_tx,
            output_rx,
        }
    }

    #[cfg(test)]
    pub(crate) fn new() -> (Self, MockNovaControl) {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (output_tx, output_rx) = mpsc::channel(NOVA_OUTPUT_QUEUE_CAPACITY);
        (
            Self {
                sent: Arc::clone(&sent),
                _output_tx: output_tx.clone(),
                output_rx,
            },
            MockNovaControl { sent, output_tx },
        )
    }
}

impl NovaSession for MockNovaSession {
    fn send(&mut self, event: NovaInputEvent) -> Result<(), VoiceRuntimeError> {
        if !valid_external_nova_event(&event) {
            return Err(VoiceRuntimeError::InvalidSignal);
        }
        self.sent
            .lock()
            .map_err(|_| VoiceRuntimeError::Closed)?
            .push(event);
        Ok(())
    }

    fn output(&mut self) -> &mut mpsc::Receiver<NovaOutputEvent> {
        &mut self.output_rx
    }

    fn ready(&mut self) -> NovaReadyFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn close(&mut self) {}
}

fn audio_input_event(
    frame: VoicePcmFrame,
    grammar: &NovaGrammar,
) -> Result<NovaInputEvent, VoiceRuntimeError> {
    if !valid_pcm_frame(&frame) {
        return Err(VoiceRuntimeError::InvalidSignal);
    }
    let mut bytes = Vec::with_capacity(frame.samples.len() * 2);
    for sample in frame.samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(NovaInputEvent::new(NovaInputEventKind::AudioInput {
        prompt_name: grammar.prompt_name.clone(),
        content_name: grammar.microphone_content_name.clone(),
        content: base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

#[cfg(test)]
pub(crate) struct MockNovaControl {
    sent: Arc<std::sync::Mutex<Vec<NovaInputEvent>>>,
    output_tx: mpsc::Sender<NovaOutputEvent>,
}

#[cfg(test)]
impl MockNovaControl {
    fn sent(&self) -> Vec<NovaInputEvent> {
        self.sent
            .lock()
            .expect("mock Nova sent lock poisoned")
            .clone()
    }

    fn emit(&self, event: NovaOutputEvent) {
        self.output_tx
            .try_send(event)
            .expect("mock Nova actor open");
    }
}

struct FakeMediaFactory;

impl VoiceMediaFactory for FakeMediaFactory {
    fn open(&self) -> VoiceMediaFuture<'_, Box<dyn VoiceMediaSession>> {
        let (event_tx, event_rx) = mpsc::channel(8);
        let session = FakeMediaSession {
            closed: false,
            event_tx,
            event_rx: Some(event_rx),
        };
        Box::pin(async move { Ok(Box::new(session) as Box<dyn VoiceMediaSession>) })
    }
}

struct FakeMediaSession {
    closed: bool,
    event_tx: mpsc::Sender<VoiceMediaEvent>,
    event_rx: Option<mpsc::Receiver<VoiceMediaEvent>>,
}

impl VoiceMediaSession for FakeMediaSession {
    fn accept_offer<'a>(&'a mut self, offer: &'a str) -> VoiceMediaFuture<'a, String> {
        Box::pin(async move {
            if self.closed || !offer.contains("m=audio") {
                return Err(VoiceRuntimeError::InvalidSignal);
            }
            self.event_tx
                .try_send(VoiceMediaEvent::Connected)
                .map_err(|_| VoiceRuntimeError::Closed)?;
            Ok("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 0\r\na=rtpmap:0 PCMU/8000\r\n".to_owned())
        })
    }

    fn add_ice_candidate<'a>(
        &'a mut self,
        _candidate: &'a VoiceIceCandidate,
    ) -> VoiceMediaFuture<'a, ()> {
        Box::pin(async move {
            (!self.closed)
                .then_some(())
                .ok_or(VoiceRuntimeError::Closed)
        })
    }

    fn end_ice_candidates(&mut self) -> VoiceMediaFuture<'_, ()> {
        Box::pin(async move {
            (!self.closed)
                .then_some(())
                .ok_or(VoiceRuntimeError::Closed)
        })
    }

    fn take_input_audio(&mut self) -> Option<mpsc::Receiver<VoicePcmFrame>> {
        None
    }

    fn take_events(&mut self) -> Option<mpsc::Receiver<VoiceMediaEvent>> {
        self.event_rx.take()
    }

    fn play_output_audio(&mut self, _frame: VoicePcmFrame) -> Result<(), VoiceRuntimeError> {
        (!self.closed)
            .then_some(())
            .ok_or(VoiceRuntimeError::Closed)
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

#[derive(Debug)]
pub(crate) enum VoiceCommand {
    Offer(String),
    IceCandidates(Vec<VoiceIceCandidate>),
    IceComplete,
    Stop(VoiceStopReason),
}

struct PendingAgentTool {
    tool_use_id: String,
    response_message_id: Option<String>,
    deadline: tokio::time::Instant,
}

struct NovaGrammar {
    prompt_name: String,
    system_content_name: String,
    microphone_content_name: String,
}

impl NovaGrammar {
    fn new() -> Self {
        Self {
            prompt_name: uuid::Uuid::new_v4().to_string(),
            system_content_name: uuid::Uuid::new_v4().to_string(),
            microphone_content_name: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn content_name(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

impl PendingAgentTool {
    fn observe_start(&mut self, event: &ChatEvent) {
        if let ChatEvent::StreamStart(start) = event
            && self.response_message_id.is_none()
        {
            self.response_message_id = start.message_id.clone();
        }
    }

    fn matches_end(&self, event: &ChatEvent) -> bool {
        let ChatEvent::StreamEnd(end) = event else {
            return false;
        };
        self.response_message_id
            .as_ref()
            .zip(end.message.message_id.as_ref())
            .is_some_and(|(start, end)| start.as_str() == end.0.as_str())
    }
}

#[derive(Clone)]
pub(crate) struct VoiceSessionHandle {
    tx: mpsc::Sender<VoiceCommand>,
    closed: Arc<AtomicBool>,
}

impl VoiceSessionHandle {
    pub(crate) fn send(&self, command: VoiceCommand) -> Result<(), VoiceRuntimeError> {
        self.tx
            .try_send(command)
            .map_err(|_| VoiceRuntimeError::Closed)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

pub(crate) fn spawn_voice_session(
    session_id: VoiceSessionId,
    target: VoiceTarget,
    agent: AgentHandle,
    output: Stream,
    settings: VoiceSettings,
    runtime: VoiceRuntime,
) -> VoiceSessionHandle {
    let (tx, rx) = mpsc::channel(VOICE_COMMAND_QUEUE_CAPACITY);
    let closed = Arc::new(AtomicBool::new(false));
    let handle = VoiceSessionHandle {
        tx,
        closed: Arc::clone(&closed),
    };
    tokio::spawn(run_voice_session(VoiceSessionResources {
        session_id,
        target,
        agent,
        output,
        settings,
        runtime,
        commands: rx,
        closed,
    }));
    handle
}

struct VoiceSessionResources {
    session_id: VoiceSessionId,
    target: VoiceTarget,
    agent: AgentHandle,
    output: Stream,
    settings: VoiceSettings,
    runtime: VoiceRuntime,
    commands: mpsc::Receiver<VoiceCommand>,
    closed: Arc<AtomicBool>,
}

async fn run_voice_session(resources: VoiceSessionResources) {
    let VoiceSessionResources {
        session_id,
        target,
        agent,
        output,
        settings,
        runtime,
        mut commands,
        closed,
    } = resources;
    struct ClosedGuard(Arc<AtomicBool>);
    impl Drop for ClosedGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let _closed_guard = ClosedGuard(closed);
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel();
    let agent_stream = Stream::new(
        StreamPath(format!("/voice-internal/{session_id}")),
        agent_tx,
    );
    let attached = tokio::time::timeout(BOOTSTRAP_TIMEOUT, agent.attach(agent_stream)).await;
    if !matches!(attached, Ok(true)) {
        emit_fatal(
            &output,
            &session_id,
            VoiceErrorCode::AgentUnavailable,
            "The selected agent is unavailable.",
        );
        return;
    }
    let bootstrap = tokio::time::timeout(BOOTSTRAP_TIMEOUT, agent_rx.recv()).await;
    let Ok(Some(envelope)) = bootstrap else {
        emit_fatal(
            &output,
            &session_id,
            VoiceErrorCode::AgentUnavailable,
            "The selected agent did not provide a bootstrap.",
        );
        return;
    };
    let Ok(bootstrap) = envelope.parse_payload::<AgentBootstrapPayload>() else {
        emit_fatal(
            &output,
            &session_id,
            VoiceErrorCode::Internal,
            "The selected agent returned an invalid bootstrap.",
        );
        return;
    };
    if envelope.kind != FrameKind::AgentBootstrap {
        emit_fatal(
            &output,
            &session_id,
            VoiceErrorCode::Internal,
            "The selected agent returned an invalid bootstrap.",
        );
        return;
    }

    let mut nova = match tokio::time::timeout(
        PROVIDER_STARTUP_TIMEOUT,
        runtime.provider.open(&settings),
    )
    .await
    {
        Ok(Ok(nova)) => nova,
        Err(_) => {
            emit_fatal(
                &output,
                &session_id,
                VoiceErrorCode::ProviderUnavailable,
                "Voice provider is unavailable.",
            );
            return;
        }
        Ok(Err(VoiceRuntimeError::Provider(failure))) => {
            emit_provider_failure(&output, &session_id, &failure);
            return;
        }
        Ok(Err(_)) => {
            emit_fatal(
                &output,
                &session_id,
                VoiceErrorCode::ProviderUnavailable,
                "Voice provider is unavailable.",
            );
            return;
        }
    };
    let mut media = match tokio::time::timeout(PROVIDER_STARTUP_TIMEOUT, runtime.media.open()).await
    {
        Ok(Ok(media)) => media,
        Ok(Err(_)) | Err(_) => {
            emit_fatal(
                &output,
                &session_id,
                VoiceErrorCode::NotAvailable,
                "Voice media is unavailable.",
            );
            return;
        }
    };
    let mut media_audio = media.take_input_audio();
    let mut media_events = media.take_events();
    let nova_grammar = NovaGrammar::new();
    if start_nova_grammar(nova.as_mut(), &nova_grammar).is_err() {
        emit_fatal(
            &output,
            &session_id,
            VoiceErrorCode::ProviderUnavailable,
            "Voice provider startup failed.",
        );
        nova.close();
        media.close();
        return;
    }
    match tokio::time::timeout(PROVIDER_STARTUP_TIMEOUT, nova.ready()).await {
        Ok(Ok(())) => {}
        Ok(Err(VoiceRuntimeError::Provider(failure))) => {
            emit_provider_failure(&output, &session_id, &failure);
            nova.close();
            media.close();
            return;
        }
        Ok(Err(_)) | Err(_) => {
            emit_fatal(
                &output,
                &session_id,
                VoiceErrorCode::ProviderUnavailable,
                "Voice provider did not establish a stream in time.",
            );
            nova.close();
            media.close();
            return;
        }
    }
    send_payload(
        &output,
        FrameKind::VoiceReady,
        &VoiceReadyPayload {
            session_id: session_id.clone(),
            target,
            direct_connections_only: true,
            expires_after_seconds: VOICE_SESSION_MAX_SECONDS,
        },
    );

    let timeout = tokio::time::sleep(Duration::from_secs(VOICE_SESSION_MAX_SECONDS));
    tokio::pin!(timeout);
    let mut pending_tool: Option<PendingAgentTool> = None;
    let mut agent_turn_active = bootstrap.turn_active;
    let mut live_event_seq = 0_u64;
    let mut offer_received = false;
    let mut candidate_count = 0_usize;
    let stop_reason;
    let mut terminal_error = false;
    loop {
        let tool_deadline = pending_tool.as_ref().map(|pending| pending.deadline);
        tokio::select! {
            _ = &mut timeout => {
                emit_fatal(&output, &session_id, VoiceErrorCode::TimedOut, "Voice session reached its time limit.");
                terminal_error = true;
                stop_reason = VoiceStopReason::TimedOut;
                break;
            }
            command = commands.recv() => {
                match command {
                    Some(VoiceCommand::Offer(offer)) if !offer_received && valid_audio_sdp(&offer) => {
                        emit_state(&output, &session_id, VoiceSessionState::Negotiating, None, None);
                        match media.accept_offer(&offer).await {
                        Ok(sdp) => {
                            if !valid_audio_sdp(&sdp) {
                                emit_fatal(&output, &session_id, VoiceErrorCode::MediaNegotiationFailed, "Voice media returned an invalid answer.");
                                terminal_error = true;
                                stop_reason = VoiceStopReason::MediaFailed;
                                break;
                            }
                            offer_received = true;
                            send_payload(&output, FrameKind::VoiceAnswer, &VoiceAnswerPayload { session_id: session_id.clone(), sdp });
                        }
                        Err(_) => {
                            emit_fatal(&output, &session_id, VoiceErrorCode::MediaNegotiationFailed, "Voice media negotiation failed.");
                            terminal_error = true;
                            stop_reason = VoiceStopReason::MediaFailed;
                            break;
                        }
                        }
                    },
                    Some(VoiceCommand::Offer(_)) => {
                        emit_fatal(&output, &session_id, VoiceErrorCode::InvalidRequest, "Voice offer was duplicate or malformed.");
                        terminal_error = true;
                        stop_reason = VoiceStopReason::MediaFailed;
                        break;
                    }
                    Some(VoiceCommand::IceCandidates(candidates)) => {
                        candidate_count = candidate_count.saturating_add(candidates.len());
                        let mut failed = !offer_received || candidate_count > MAX_VOICE_ICE_CANDIDATES;
                        if !failed {
                            for candidate in &candidates {
                                if media.add_ice_candidate(candidate).await.is_err() {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                        if failed {
                            emit_fatal(&output, &session_id, VoiceErrorCode::MediaNegotiationFailed, "Voice ICE signaling failed.");
                            terminal_error = true;
                            stop_reason = VoiceStopReason::MediaFailed;
                            break;
                        }
                    }
                    Some(VoiceCommand::IceComplete) => {
                        if media.end_ice_candidates().await.is_err() {
                            emit_fatal(&output, &session_id, VoiceErrorCode::MediaNegotiationFailed, "Voice ICE signaling failed.");
                            terminal_error = true;
                            stop_reason = VoiceStopReason::MediaFailed;
                            break;
                        }
                    }
                    Some(VoiceCommand::Stop(reason)) => {
                        stop_reason = reason;
                        break;
                    }
                    None => {
                        stop_reason = VoiceStopReason::ClientGone;
                        break;
                    }
                }
            }
            agent_event = agent_rx.recv() => {
                let Some(envelope) = agent_event else {
                    emit_fatal(&output, &session_id, VoiceErrorCode::AgentUnavailable, "The selected agent closed.");
                    terminal_error = true;
                    stop_reason = VoiceStopReason::AgentClosed;
                    break;
                };
                if envelope.kind != FrameKind::ChatEvent {
                    continue;
                }
                let Ok(event) = envelope.parse_payload::<ChatEvent>() else { continue; };
                match &event {
                    ChatEvent::StreamStart(_) => agent_turn_active = true,
                    ChatEvent::StreamEnd(_) => agent_turn_active = false,
                    _ => {}
                }
                live_event_seq = live_event_seq.saturating_add(1);
                if let Some(pending) = pending_tool.as_mut() {
                    pending.observe_start(&event);
                }
                let correlated_end = pending_tool
                    .as_ref()
                    .is_some_and(|pending| pending.matches_end(&event));
                if !correlated_end && let Some(kind) = project_progress(&event) {
                    let progress = VoiceAgentProgress { source_seq: live_event_seq, source_kind: kind };
                    emit_state(&output, &session_id, VoiceSessionState::AgentWorking, Some(progress.clone()), None);
                    if send_progress_fact(nova.as_mut(), &nova_grammar, &progress).is_err() {
                        emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider rejected agent progress.");
                        terminal_error = true;
                        stop_reason = VoiceStopReason::MediaFailed;
                        break;
                    }
                }
                if correlated_end {
                    let ChatEvent::StreamEnd(end) = event else { unreachable!() };
                    let tool_use_id = pending_tool.take().expect("correlated pending tool").tool_use_id;
                    let text = if end.message.content.trim().is_empty() {
                        "The agent completed the request.".to_owned()
                    } else {
                        bounded_utf8(&end.message.content, 32 * 1024)
                    };
                    if send_tool_result(nova.as_mut(), &nova_grammar, tool_use_id, true, &text).is_err() {
                        emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider rejected the completed tool result.");
                        terminal_error = true;
                        stop_reason = VoiceStopReason::MediaFailed;
                        break;
                    }
                    emit_state(&output, &session_id, VoiceSessionState::Listening, None, None);
                }
            }            audio = receive_media_audio(&mut media_audio) => {
                let Some(audio) = audio else {
                    media_audio = None;
                    continue;
                };
                if !valid_pcm_frame(&audio)
                    || audio_input_event(audio, &nova_grammar)
                        .and_then(|event| nova.send(event))
                        .is_err()
                {
                    emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider audio input failed.");
                    terminal_error = true;
                    stop_reason = VoiceStopReason::MediaFailed;
                    break;
                }
            }
            media_event = receive_media_event(&mut media_events) => {
                match media_event {
                    Some(VoiceMediaEvent::Connected) => {
                        emit_state(&output, &session_id, VoiceSessionState::Connected, None, None);
                        emit_state(&output, &session_id, VoiceSessionState::Listening, None, None);
                    }
                    Some(VoiceMediaEvent::Failed) | None => {
                        emit_fatal(&output, &session_id, VoiceErrorCode::MediaNegotiationFailed, "Voice media connection failed.");
                        terminal_error = true;
                        stop_reason = VoiceStopReason::MediaFailed;
                        break;
                    }
                }
            }
            _ = pending_tool_deadline(tool_deadline) => {
                let pending = pending_tool.take().expect("pending deadline armed");
                if send_tool_result(
                    nova.as_mut(),
                    &nova_grammar,
                    pending.tool_use_id,
                    false,
                    "The Tyde agent did not complete the request in time.",
                ).is_err() {
                    emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider rejected the timed-out tool result.");
                    terminal_error = true;
                    stop_reason = VoiceStopReason::MediaFailed;
                    break;
                }
                emit_error(
                    &output,
                    &session_id,
                    VoiceErrorCode::ToolDeliveryFailed,
                    "The selected agent did not complete the request in time.",
                    false,
                );
                emit_state(&output, &session_id, VoiceSessionState::Listening, None, None);
            }
            nova_event = nova.output().recv() => {
                let Some(nova_event) = nova_event else {
                    emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider closed unexpectedly.");
                    terminal_error = true;
                    stop_reason = VoiceStopReason::MediaFailed;
                    break;
                };
                match nova_event {
                    NovaOutputEvent::ProviderFailed(failure) => {
                        emit_provider_failure(&output, &session_id, &failure);
                        terminal_error = true;
                        stop_reason = VoiceStopReason::MediaFailed;
                        break;
                    }
                    NovaOutputEvent::ToolUse { tool_use_id, name, input } => {
                        if tool_use_id.is_empty() || tool_use_id.len() > 256 {
                            emit_error(&output, &session_id, VoiceErrorCode::InvalidRequest, "Voice tool request was rejected.", false);
                            continue;
                        }
                        if let Some(code) = tool_request_rejection(
                            &name,
                            pending_tool.as_ref().map(|pending| pending.tool_use_id.as_str()),
                            agent_turn_active,
                        ) {
                            emit_error(&output, &session_id, code, "Voice tool request was rejected.", false);
                            if send_tool_result(nova.as_mut(), &nova_grammar, tool_use_id, false, "Tool request rejected.").is_err() {
                                emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider rejected the tool result.");
                                terminal_error = true;
                                stop_reason = VoiceStopReason::MediaFailed;
                                break;
                            }
                            continue;
                        }
                        let Ok(message) = parse_agent_tool_message(&input) else {
                            emit_error(&output, &session_id, VoiceErrorCode::InvalidRequest, "Voice tool message was invalid.", false);
                            if send_tool_result(nova.as_mut(), &nova_grammar, tool_use_id, false, "Invalid message.").is_err() {
                                emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider rejected the tool result.");
                                terminal_error = true;
                                stop_reason = VoiceStopReason::MediaFailed;
                                break;
                            }
                            continue;
                        };
                        let delivery = agent
                            .deliver_message(agent_message_payload(message))
                            .await;
                        if delivery.is_err() {
                            emit_error(&output, &session_id, VoiceErrorCode::ToolDeliveryFailed, "The selected agent could not accept the request.", false);
                            if send_tool_result(nova.as_mut(), &nova_grammar, tool_use_id, false, "Agent unavailable.").is_err() {
                                emit_fatal(&output, &session_id, VoiceErrorCode::ProviderUnavailable, "Voice provider rejected the tool result.");
                                terminal_error = true;
                                stop_reason = VoiceStopReason::MediaFailed;
                                break;
                            }
                        } else {
                            pending_tool = Some(PendingAgentTool {
                                tool_use_id,
                                response_message_id: None,
                                deadline: tokio::time::Instant::now() + PENDING_TOOL_TIMEOUT,
                            });
                            emit_state(&output, &session_id, VoiceSessionState::AgentWorking, None, None);
                        }
                    }
                    NovaOutputEvent::Interrupted {} => {
                        emit_state(&output, &session_id, VoiceSessionState::Listening, None, None);
                    }
                    NovaOutputEvent::AudioOutput(frame) => {
                        emit_state(&output, &session_id, VoiceSessionState::Speaking, None, None);
                        if !valid_pcm_frame(&frame) || media.play_output_audio(frame).is_err() {
                            emit_fatal(&output, &session_id, VoiceErrorCode::MediaNegotiationFailed, "Voice audio playback failed.");
                            terminal_error = true;
                            stop_reason = VoiceStopReason::MediaFailed;
                            break;
                        }
                    }
                    NovaOutputEvent::Transcript { speaker, text, is_final } => {
                        let text = bounded_utf8(text.trim(), 4 * 1024);
                        if !text.is_empty() {
                            send_payload(
                                &output,
                                FrameKind::VoiceState,
                                &VoiceStatePayload {
                                    session_id: session_id.clone(),
                                    state: if speaker == VoiceTranscriptSpeaker::Assistant {
                                        VoiceSessionState::Speaking
                                    } else {
                                        VoiceSessionState::Listening
                                    },
                                    progress: None,
                                    caption: Some(text.clone()),
                                    transcript: Some(VoiceTranscript { speaker, text, is_final }),
                                    ended_reason: None,
                                },
                            );
                        }
                    }
                }
            }

        }
    }
    if !terminal_error {
        emit_state(&output, &session_id, VoiceSessionState::Ending, None, None);
    }
    if let Some(pending) = pending_tool.take() {
        let _ = send_tool_result(
            nova.as_mut(),
            &nova_grammar,
            pending.tool_use_id,
            false,
            "The voice session ended before the Tyde agent completed the request.",
        );
    }
    let _ = finish_nova_grammar(nova.as_mut(), &nova_grammar);
    nova.close();
    media.close();
    if !terminal_error {
        emit_state(
            &output,
            &session_id,
            VoiceSessionState::Ended,
            None,
            Some(stop_reason),
        );
    }
}

async fn receive_media_audio(
    receiver: &mut Option<mpsc::Receiver<VoicePcmFrame>>,
) -> Option<VoicePcmFrame> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn receive_media_event(
    receiver: &mut Option<mpsc::Receiver<VoiceMediaEvent>>,
) -> Option<VoiceMediaEvent> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn pending_tool_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn parse_agent_tool_message(input: &serde_json::Value) -> Result<&str, VoiceErrorCode> {
    let object = input.as_object().ok_or(VoiceErrorCode::InvalidRequest)?;
    if object.len() != 1 {
        return Err(VoiceErrorCode::InvalidRequest);
    }
    let message = object
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| {
            !message.trim().is_empty() && message.len() <= MAX_VOICE_TOOL_MESSAGE_BYTES
        })
        .ok_or(VoiceErrorCode::InvalidRequest)?;
    Ok(message)
}

fn tool_request_rejection(
    name: &str,
    pending: Option<&str>,
    agent_turn_active: bool,
) -> Option<VoiceErrorCode> {
    if pending.is_some() || agent_turn_active {
        Some(VoiceErrorCode::ToolBusy)
    } else if name != AGENT_TOOL_NAME {
        Some(VoiceErrorCode::InvalidRequest)
    } else {
        None
    }
}

fn agent_message_payload(message: &str) -> SendMessagePayload {
    SendMessagePayload {
        message: message.to_owned(),
        images: None,
        origin: Some(MessageOrigin::User),
        tool_response: None,
    }
}

fn bounded_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn start_nova_grammar(
    nova: &mut dyn NovaSession,
    grammar: &NovaGrammar,
) -> Result<(), VoiceRuntimeError> {
    nova.send(NovaInputEvent::new(NovaInputEventKind::SessionStart {
        inference_configuration: serde_json::json!({
            "maxTokens": 1024,
            "topP": 0.9,
            "temperature": 0.7
        }),
        turn_detection_configuration: serde_json::json!({
            "endpointingSensitivity": "MEDIUM"
        }),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::PromptStart {
        prompt_name: grammar.prompt_name.clone(),
        audio_output_configuration: NovaAudioOutputConfiguration {
            media_type: "audio/lpcm".to_owned(),
            sample_rate_hertz: 24_000,
            sample_size_bits: 16,
            channel_count: 1,
            voice_id: "tiffany".to_owned(),
            encoding: "base64".to_owned(),
            audio_type: "SPEECH".to_owned(),
        },
        text_output_configuration: serde_json::json!({"mediaType": "text/plain"}),
        tool_use_output_configuration: serde_json::json!({"mediaType": "application/json"}),
        tool_configuration: serde_json::json!({"tools": [{"toolSpec": NovaToolSpecification {
            name: AGENT_TOOL_NAME.to_owned(),
            description: "Send one ordinary user message to the fixed Tyde agent.".to_owned(),
            input_schema: serde_json::json!({"json": {
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"],
                "additionalProperties": false
            }}),
        }}], "toolChoice": {"auto": {}}}),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentStart {
        prompt_name: grammar.prompt_name.clone(),
        content_name: grammar.system_content_name.clone(),
        role: "SYSTEM".to_owned(),
        content_type: "TEXT".to_owned(),
        interactive: false,
        configuration: serde_json::Map::from_iter([(
            "textInputConfiguration".to_owned(),
            serde_json::json!({"mediaType": "text/plain"}),
        )]),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::TextInput {
        prompt_name: grammar.prompt_name.clone(),
        content_name: grammar.system_content_name.clone(),
        content: "Use the available tool for requests that need the user's fixed Tyde agent."
            .to_owned(),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentEnd {
        prompt_name: grammar.prompt_name.clone(),
        content_name: grammar.system_content_name.clone(),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentStart {
        prompt_name: grammar.prompt_name.clone(),
        content_name: grammar.microphone_content_name.clone(),
        role: "USER".to_owned(),
        content_type: "AUDIO".to_owned(),
        interactive: true,
        configuration: serde_json::Map::from_iter([(
            "audioInputConfiguration".to_owned(),
            serde_json::json!({
                "mediaType": "audio/lpcm",
                "sampleRateHertz": 16000,
                "sampleSizeBits": 16,
                "channelCount": 1,
                "audioType": "SPEECH",
                "encoding": "base64"
            }),
        )]),
    }))
}

fn finish_nova_grammar(
    nova: &mut dyn NovaSession,
    grammar: &NovaGrammar,
) -> Result<(), VoiceRuntimeError> {
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentEnd {
        prompt_name: grammar.prompt_name.clone(),
        content_name: grammar.microphone_content_name.clone(),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::PromptEnd {
        prompt_name: grammar.prompt_name.clone(),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::SessionEnd {}))
}

fn send_tool_result(
    nova: &mut dyn NovaSession,
    grammar: &NovaGrammar,
    tool_use_id: String,
    success: bool,
    text: &str,
) -> Result<(), VoiceRuntimeError> {
    let content_name = grammar.content_name();
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentStart {
        prompt_name: grammar.prompt_name.clone(),
        content_name: content_name.clone(),
        role: "TOOL".to_owned(),
        content_type: "TOOL".to_owned(),
        interactive: false,
        configuration: serde_json::Map::from_iter([(
            "toolResultInputConfiguration".to_owned(),
            serde_json::json!({"toolUseId": tool_use_id, "type": "TEXT", "textInputConfiguration": {"mediaType": "text/plain"}}),
        )]),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::ToolResult {
        prompt_name: grammar.prompt_name.clone(),
        content_name: content_name.clone(),
        content: serde_json::json!({"success": success, "text": text}).to_string(),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentEnd {
        prompt_name: grammar.prompt_name.clone(),
        content_name,
    }))
}

fn send_progress_fact(
    nova: &mut dyn NovaSession,
    grammar: &NovaGrammar,
    progress: &VoiceAgentProgress,
) -> Result<(), VoiceRuntimeError> {
    let content_name = grammar.content_name();
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentStart {
        prompt_name: grammar.prompt_name.clone(),
        content_name: content_name.clone(),
        role: "USER".to_owned(),
        content_type: "TEXT".to_owned(),
        interactive: true,
        configuration: serde_json::Map::from_iter([(
            "textInputConfiguration".to_owned(),
            serde_json::json!({"mediaType": "text/plain"}),
        )]),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::TextInput {
        prompt_name: grammar.prompt_name.clone(),
        content_name: content_name.clone(),
        content: format!("Tyde agent event: {:?}.", progress.source_kind),
    }))?;
    nova.send(NovaInputEvent::new(NovaInputEventKind::ContentEnd {
        prompt_name: grammar.prompt_name.clone(),
        content_name,
    }))
}

pub(crate) fn project_progress(event: &ChatEvent) -> Option<VoiceAgentProgressKind> {
    match event {
        ChatEvent::StreamStart(_) => Some(VoiceAgentProgressKind::ResponseStarted),
        ChatEvent::ToolRequest(_) => Some(VoiceAgentProgressKind::ToolStarted),
        ChatEvent::ToolProgress(_) => Some(VoiceAgentProgressKind::ToolProgressed),
        ChatEvent::TaskUpdate(_) => Some(VoiceAgentProgressKind::TaskListChanged),
        ChatEvent::RetryAttempt(_) => Some(VoiceAgentProgressKind::Retrying),
        ChatEvent::StreamEnd(_) => Some(VoiceAgentProgressKind::ResponseCompleted),
        ChatEvent::MessageAdded(_)
        | ChatEvent::MessageMetadataUpdated(_)
        | ChatEvent::TypingStatusChanged(_)
        | ChatEvent::StreamDelta(_)
        | ChatEvent::StreamReasoningDelta(_)
        | ChatEvent::ToolExecutionCompleted(_)
        | ChatEvent::OperationCancelled(_)
        | ChatEvent::Orchestration(_)
        | ChatEvent::ContextCompaction(_) => None,
    }
}

fn emit_state(
    output: &Stream,
    session_id: &VoiceSessionId,
    state: VoiceSessionState,
    progress: Option<VoiceAgentProgress>,
    ended_reason: Option<VoiceStopReason>,
) {
    send_payload(
        output,
        FrameKind::VoiceState,
        &VoiceStatePayload {
            session_id: session_id.clone(),
            state,
            progress,
            caption: None,
            transcript: None,
            ended_reason,
        },
    );
}

fn emit_error(
    output: &Stream,
    session_id: &VoiceSessionId,
    code: VoiceErrorCode,
    message: &str,
    fatal: bool,
) {
    send_payload(
        output,
        FrameKind::VoiceError,
        &VoiceErrorPayload {
            session_id: session_id.clone(),
            code,
            message: message.to_owned(),
            fatal,
        },
    );
}

fn emit_fatal(output: &Stream, session_id: &VoiceSessionId, code: VoiceErrorCode, message: &str) {
    emit_error(output, session_id, code, message, true);
}

fn emit_provider_failure(output: &Stream, session_id: &VoiceSessionId, failure: &ProviderFailure) {
    let message = match failure.category.as_str() {
        "access_denied" => {
            "AWS denied Nova access. Check the selected profile and Bedrock permissions."
        }
        "model_unavailable" => "Nova 2 Sonic is not available for this AWS account or region.",
        "quota" => "AWS throttled the Nova session or its service quota is exhausted.",
        "invalid_request" => "AWS rejected the Nova session configuration.",
        "timeout" => "The Nova stream timed out.",
        "service_unavailable" => "Amazon Bedrock is temporarily unavailable.",
        "credentials_or_transport" => {
            "AWS credentials could not be used or Bedrock could not be reached. Check the selected profile, region, and network."
        }
        "stream_transport" => "The connection to Amazon Bedrock was interrupted.",
        "stream_closed" => "Amazon Bedrock closed the Nova stream unexpectedly.",
        "invalid_response" => "Amazon Bedrock returned an invalid Nova event.",
        _ => "The Nova provider failed unexpectedly.",
    };
    emit_fatal(output, session_id, failure.code, message);
}

fn send_payload<T: Serialize>(output: &Stream, kind: FrameKind, payload: &T) {
    if let Ok(value) = serde_json::to_value(payload) {
        let _ = output.send_value(kind, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        AgentControlOutput, AgentOrigin, AgentStartPayload, BackendKind, ChatMessage,
        MessageSender, ReasoningData, StreamEndData, StreamStartData, StreamTextDeltaData,
    };
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn mock_nova_grammar_is_balanced_and_correlated() {
        let (mut nova, control) = MockNovaSession::new();
        let grammar = NovaGrammar::new();
        start_nova_grammar(&mut nova, &grammar).expect("start grammar");
        send_tool_result(&mut nova, &grammar, "tool-1".to_owned(), true, "done")
            .expect("tool grammar");
        finish_nova_grammar(&mut nova, &grammar).expect("finish grammar");
        assert!(uuid::Uuid::parse_str(&grammar.prompt_name).is_ok());
        let sent = control.sent();
        assert!(matches!(
            sent.first().map(|event| &event.event),
            Some(NovaInputEventKind::SessionStart { .. })
        ));
        assert!(matches!(
            sent.last().map(|event| &event.event),
            Some(NovaInputEventKind::SessionEnd {})
        ));
        let prompt_start = sent
            .iter()
            .find(|event| matches!(event.event, NovaInputEventKind::PromptStart { .. }))
            .expect("promptStart event");
        let prompt_start = serde_json::to_value(prompt_start).expect("serialize promptStart");
        assert_eq!(
            prompt_start["event"]["promptStart"]["toolConfiguration"],
            serde_json::json!({
                "tools": [{
                    "toolSpec": {
                        "name": "send_to_focused_tyde_agent",
                        "description": "Send one ordinary user message to the fixed Tyde agent.",
                        "inputSchema": {
                            "json": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"],
                                "additionalProperties": false
                            }
                        }
                    }
                }],
                "toolChoice": {"auto": {}}
            })
        );
        let mut open = Vec::new();
        for event in sent {
            match event.event {
                NovaInputEventKind::ContentStart {
                    prompt_name,
                    content_name,
                    ..
                } => {
                    assert_eq!(prompt_name, grammar.prompt_name);
                    open.push(content_name);
                }
                NovaInputEventKind::TextInput {
                    prompt_name,
                    content_name,
                    ..
                }
                | NovaInputEventKind::ToolResult {
                    prompt_name,
                    content_name,
                    ..
                }
                | NovaInputEventKind::AudioInput {
                    prompt_name,
                    content_name,
                    ..
                } => {
                    assert_eq!(prompt_name, grammar.prompt_name);
                    assert_eq!(open.last(), Some(&content_name));
                }
                NovaInputEventKind::ContentEnd {
                    prompt_name,
                    content_name,
                } => {
                    assert_eq!(prompt_name, grammar.prompt_name);
                    assert_eq!(open.pop(), Some(content_name));
                }
                _ => {}
            }
        }
        assert!(open.is_empty());
        assert_eq!(
            serde_json::to_value(NovaInputEvent::new(NovaInputEventKind::SessionStart {
                inference_configuration: serde_json::json!({"maxTokens": 1024}),
                turn_detection_configuration: serde_json::json!({"endpointingSensitivity": "MEDIUM"}),
            }))
            .expect("serialize session start"),
            serde_json::json!({"event": {"sessionStart": {
                "inferenceConfiguration": {"maxTokens": 1024},
                "turnDetectionConfiguration": {"endpointingSensitivity": "MEDIUM"}
            }}})
        );
    }

    #[test]
    fn progress_projection_excludes_text_reasoning_and_timers() {
        let delta = ChatEvent::StreamDelta(StreamTextDeltaData {
            message_id: Some("message-1".to_owned()),
            text: "private response text".to_owned(),
        });
        let reasoning = ChatEvent::MessageAdded(ChatMessage {
            message_id: None,
            timestamp: 0,
            sender: MessageSender::Assistant {
                agent: "agent".to_owned(),
            },
            content: String::new(),
            reasoning: Some(ReasoningData {
                text: "private reasoning".to_owned(),
                tokens: None,
                signature: None,
                blob: None,
            }),
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        });
        assert_eq!(project_progress(&delta), None);
        assert_eq!(project_progress(&reasoning), None);
    }

    #[test]
    fn mock_nova_control_delivers_events_without_network() {
        let (mut nova, control) = MockNovaSession::new();
        control.emit(NovaOutputEvent::Interrupted {});
        assert!(matches!(
            nova.output_rx.try_recv(),
            Ok(NovaOutputEvent::Interrupted {})
        ));
    }

    #[test]
    fn agent_tool_input_cannot_retarget_the_immutable_session() {
        assert_eq!(
            parse_agent_tool_message(&serde_json::json!({
                "message": "do the work",
                "agent_id": "different-agent"
            })),
            Err(VoiceErrorCode::InvalidRequest)
        );
        assert_eq!(
            parse_agent_tool_message(&serde_json::json!({"message": "do the work"})),
            Ok("do the work")
        );
        assert_eq!(
            tool_request_rejection(AGENT_TOOL_NAME, Some("tool-1"), false),
            Some(VoiceErrorCode::ToolBusy)
        );
        assert_eq!(
            tool_request_rejection(AGENT_TOOL_NAME, None, true),
            Some(VoiceErrorCode::ToolBusy)
        );
        assert_eq!(tool_request_rejection(AGENT_TOOL_NAME, None, false), None);
        let delivered = agent_message_payload("ordinary request");
        assert_eq!(delivered.message, "ordinary request");
        assert_eq!(delivered.origin, Some(MessageOrigin::User));
        assert!(delivered.images.is_none());
        assert!(delivered.tool_response.is_none());
    }

    #[test]
    fn pending_tool_ignores_an_unrelated_in_flight_turn_end() {
        let mut pending = PendingAgentTool {
            tool_use_id: "tool-1".to_owned(),
            response_message_id: None,
            deadline: tokio::time::Instant::now() + PENDING_TOOL_TIMEOUT,
        };
        let unrelated = ChatEvent::StreamEnd(StreamEndData {
            message: assistant_message("old-turn", "unrelated"),
        });
        assert!(!pending.matches_end(&unrelated));

        pending.observe_start(&ChatEvent::StreamStart(StreamStartData {
            message_id: Some("voice-turn".to_owned()),
            agent: "agent".to_owned(),
            model: None,
        }));
        assert!(!pending.matches_end(&unrelated));
        assert!(pending.matches_end(&ChatEvent::StreamEnd(StreamEndData {
            message: assistant_message("voice-turn", "correlated"),
        })));
    }

    fn assistant_message(message_id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            message_id: Some(protocol::ChatMessageId(message_id.to_owned())),
            timestamp: 0,
            sender: MessageSender::Assistant {
                agent: "agent".to_owned(),
            },
            content: content.to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    #[test]
    fn pcm_frames_are_bounded_and_remain_internal() {
        assert!(valid_pcm_frame(&VoicePcmFrame {
            sample_rate_hertz: 16_000,
            samples: vec![0; MAX_PCM_SAMPLES_PER_FRAME],
        }));
        assert!(!valid_pcm_frame(&VoicePcmFrame {
            sample_rate_hertz: 16_000,
            samples: vec![0; MAX_PCM_SAMPLES_PER_FRAME + 1],
        }));
        assert!(!valid_pcm_frame(&VoicePcmFrame {
            sample_rate_hertz: 44_100,
            samples: vec![0; 320],
        }));
    }

    #[tokio::test]
    async fn run_voice_session_emits_ready_and_ordered_teardown() {
        let start = AgentStartPayload {
            agent_id: protocol::AgentId("agent-1".to_owned()),
            name: "Agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp".to_owned()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
        };
        let agent = crate::agent::voice_test_handle(
            start.clone(),
            AgentBootstrapPayload {
                events: vec![protocol::AgentBootstrapEvent::AgentStart(start)],
                latest_output: AgentControlOutput::Empty,
                turn_active: false,
            },
        );
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/voice/session-1".to_owned()), output_tx);
        let (command_tx, command_rx) = mpsc::channel(4);
        command_tx
            .send(VoiceCommand::Stop(VoiceStopReason::UserExited))
            .await
            .expect("queue stop");
        run_voice_session(VoiceSessionResources {
            session_id: VoiceSessionId("session-1".to_owned()),
            target: VoiceTarget {
                agent_id: protocol::AgentId("agent-1".to_owned()),
                instance_stream: StreamPath("/agent/agent-1/instance-1".to_owned()),
            },
            agent,
            output,
            settings: VoiceSettings::default(),
            runtime: VoiceRuntime::mock(),
            commands: command_rx,
            closed: Arc::new(AtomicBool::new(false)),
        })
        .await;
        let envelopes: Vec<_> = std::iter::from_fn(|| output_rx.try_recv().ok()).collect();
        let kinds: Vec<_> = envelopes.iter().map(|envelope| envelope.kind).collect();
        assert_eq!(
            kinds,
            vec![
                FrameKind::VoiceReady,
                FrameKind::VoiceState,
                FrameKind::VoiceState
            ]
        );
        let states: Vec<_> = envelopes
            .iter()
            .filter(|envelope| envelope.kind == FrameKind::VoiceState)
            .map(|envelope| {
                envelope
                    .parse_payload::<VoiceStatePayload>()
                    .expect("VoiceState")
            })
            .map(|payload| payload.state)
            .collect();
        assert_eq!(
            states,
            vec![VoiceSessionState::Ending, VoiceSessionState::Ended]
        );
    }

    struct AudioNovaProvider;

    impl NovaProvider for AudioNovaProvider {
        fn open<'a>(&'a self, _settings: &'a VoiceSettings) -> NovaOpenFuture<'a> {
            Box::pin(async {
                let (output_tx, output_rx) = mpsc::channel(4);
                output_tx
                    .try_send(NovaOutputEvent::AudioOutput(VoicePcmFrame {
                        sample_rate_hertz: 24_000,
                        samples: vec![0x5152; 480],
                    }))
                    .expect("seed synthetic audio");
                Ok(Box::new(AudioNovaSession {
                    output_tx: Some(output_tx),
                    output_rx,
                }) as Box<dyn NovaSession>)
            })
        }
    }

    struct ToolNovaProvider;

    impl NovaProvider for ToolNovaProvider {
        fn open<'a>(&'a self, _settings: &'a VoiceSettings) -> NovaOpenFuture<'a> {
            Box::pin(async {
                let (output_tx, output_rx) = mpsc::channel(4);
                output_tx
                    .try_send(NovaOutputEvent::ToolUse {
                        tool_use_id: "tool-timeout".to_owned(),
                        name: AGENT_TOOL_NAME.to_owned(),
                        input: serde_json::json!({"message": "long task"}),
                    })
                    .expect("seed tool request");
                Ok(Box::new(AudioNovaSession {
                    output_tx: Some(output_tx),
                    output_rx,
                }) as Box<dyn NovaSession>)
            })
        }
    }

    struct AudioNovaSession {
        output_tx: Option<mpsc::Sender<NovaOutputEvent>>,
        output_rx: mpsc::Receiver<NovaOutputEvent>,
    }

    impl NovaSession for AudioNovaSession {
        fn send(&mut self, _event: NovaInputEvent) -> Result<(), VoiceRuntimeError> {
            Ok(())
        }

        fn output(&mut self) -> &mut mpsc::Receiver<NovaOutputEvent> {
            &mut self.output_rx
        }

        fn ready(&mut self) -> NovaReadyFuture<'_> {
            Box::pin(async { Ok(()) })
        }

        fn close(&mut self) {
            self.output_tx.take();
        }
    }

    fn test_agent() -> AgentHandle {
        let start = AgentStartPayload {
            agent_id: protocol::AgentId("agent-1".to_owned()),
            name: "Agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Codex,
            launch_profile_id: None,
            workspace_roots: vec!["/tmp".to_owned()],
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            workflow: None,
            created_at_ms: 1,
        };
        crate::agent::voice_test_handle(
            start.clone(),
            AgentBootstrapPayload {
                events: vec![protocol::AgentBootstrapEvent::AgentStart(start)],
                latest_output: AgentControlOutput::Empty,
                turn_active: false,
            },
        )
    }

    fn test_resources(
        agent: AgentHandle,
        output: Stream,
        runtime: VoiceRuntime,
        commands: mpsc::Receiver<VoiceCommand>,
    ) -> VoiceSessionResources {
        VoiceSessionResources {
            session_id: VoiceSessionId("session-audio".to_owned()),
            target: VoiceTarget {
                agent_id: protocol::AgentId("agent-1".to_owned()),
                instance_stream: StreamPath("/agent/agent-1/instance-1".to_owned()),
            },
            agent,
            output,
            settings: VoiceSettings::default(),
            runtime,
            commands,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_tool_deadline_returns_failure_without_wedging_the_actor() {
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/voice/session-audio".to_owned()), output_tx);
        let (command_tx, command_rx) = mpsc::channel(4);
        let actor = tokio::spawn(run_voice_session(test_resources(
            test_agent(),
            output,
            VoiceRuntime {
                provider: Arc::new(ToolNovaProvider),
                media: Arc::new(FakeMediaFactory),
                available: true,
            },
            command_rx,
        )));
        loop {
            let envelope = output_rx.recv().await.expect("voice output");
            if envelope.kind == FrameKind::VoiceState
                && envelope
                    .parse_payload::<VoiceStatePayload>()
                    .is_ok_and(|payload| payload.state == VoiceSessionState::AgentWorking)
            {
                break;
            }
        }
        tokio::time::advance(PENDING_TOOL_TIMEOUT).await;
        let error = loop {
            let envelope = output_rx.recv().await.expect("timeout output");
            if envelope.kind == FrameKind::VoiceError {
                break envelope
                    .parse_payload::<VoiceErrorPayload>()
                    .expect("VoiceError");
            }
        };
        assert_eq!(error.code, VoiceErrorCode::ToolDeliveryFailed);
        assert!(!error.fatal);
        command_tx
            .send(VoiceCommand::Stop(VoiceStopReason::UserExited))
            .await
            .expect("stop voice actor");
        actor.await.expect("voice actor");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn synthetic_audio_never_reaches_the_mqtt_production_boundary() {
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/voice/session-audio".to_owned()), output_tx);
        let (command_tx, command_rx) = mpsc::channel(4);
        let actor = tokio::spawn(run_voice_session(test_resources(
            test_agent(),
            output,
            VoiceRuntime {
                provider: Arc::new(AudioNovaProvider),
                media: Arc::new(FakeMediaFactory),
                available: true,
            },
            command_rx,
        )));

        let mut envelopes = Vec::new();
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), output_rx.recv())
                .await
                .expect("voice output timeout")
                .expect("voice output closed");
            let speaking = envelope.kind == FrameKind::VoiceState
                && envelope
                    .parse_payload::<VoiceStatePayload>()
                    .is_ok_and(|payload| payload.state == VoiceSessionState::Speaking);
            envelopes.push(envelope);
            if speaking {
                break;
            }
        }
        command_tx
            .send(VoiceCommand::Stop(VoiceStopReason::UserExited))
            .await
            .expect("stop voice actor");
        actor.await.expect("voice actor");
        envelopes.extend(std::iter::from_fn(|| output_rx.try_recv().ok()));

        let (mut mqtt, mut probe) = mqtt_transport::production_boundary_probe();
        let mut published = Vec::new();
        for envelope in &envelopes {
            let mut wire = serde_json::to_vec(envelope).expect("encode voice control");
            wire.push(b'\n');
            mqtt.write_all(&wire).await.expect("write MQTT stream");
            let (flushed, bytes) = tokio::join!(mqtt.flush(), probe.next_bytes());
            flushed.expect("flush MQTT stream");
            let bytes = bytes.expect("published MQTT chunk");
            assert_eq!(bytes, wire);
            published.extend_from_slice(&bytes);
        }
        assert!(
            envelopes.iter().any(|envelope| {
                envelope.kind == FrameKind::VoiceState
                    && envelope
                        .parse_payload::<VoiceStatePayload>()
                        .is_ok_and(|payload| payload.state == VoiceSessionState::Speaking)
            }),
            "the synthetic provider audio must drive the real actor"
        );
        assert!(!published.windows(9).any(|bytes| bytes == b"audioInput"));
        let pcm_marker = base64::engine::general_purpose::STANDARD.encode([0x52, 0x51].repeat(24));
        assert!(
            !published
                .windows(pcm_marker.len())
                .any(|bytes| bytes == pcm_marker.as_bytes())
        );
    }
}
