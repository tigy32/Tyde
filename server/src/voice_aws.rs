use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll};

use aws_sdk_bedrockruntime::types::{
    BidirectionalInputPayloadPart, InvokeModelWithBidirectionalStreamInput,
};
use aws_smithy_types::Blob;
use base64::Engine;
use futures_util::Stream as FuturesStream;
use tokio::sync::{mpsc, oneshot};

use crate::voice::{
    NovaInput, NovaOutput, NovaProvider, NovaSend, NovaSession, ProviderFailure, ProviderFuture,
};

const INPUT_CAPACITY: usize = 100;
const OUTPUT_CAPACITY: usize = 32;
const TOOL_NAME: &str = "send_to_focused_tyde_agent";
const SYSTEM_PROMPT: &str = "You are Tyde's voice assistant, the spoken interface to a coding \
agent. For any substantive request — code changes, analysis, running commands, or questions \
about the project — call send_to_focused_tyde_agent with a clear, complete message for the \
agent, then briefly tell the user what you sent. Relay tool results conversationally. Keep \
every spoken reply short.";

pub(crate) struct AwsNovaProvider;

impl NovaProvider for AwsNovaProvider {
    fn open<'a>(
        &'a self,
        settings: &'a settings_model::VoiceSettings,
        session: &'a protocol::VoiceSessionId,
    ) -> ProviderFuture<'a, Box<dyn NovaSession>> {
        Box::pin(async move {
            let region = settings
                .aws_region
                .as_ref()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| {
                    failure(
                        protocol::VoiceErrorCode::NotAvailable,
                        false,
                        "region_missing",
                    )
                })?;
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new(region.clone()));
            if let Some(profile) = settings
                .aws_profile
                .as_ref()
                .filter(|v| !v.trim().is_empty())
            {
                loader = loader.profile_name(profile);
            }
            let config = loader.load().await;
            let client = aws_sdk_bedrockruntime::Client::new(&config);
            let (input_tx, input_rx) = mpsc::channel(INPUT_CAPACITY);
            let (output_tx, output_rx) = mpsc::channel(OUTPUT_CAPACITY);
            let closing = Arc::new(AtomicBool::new(false));
            let output_generation = Arc::new(AtomicU64::new(0));
            let grammar = Grammar::new(session, settings.endpointing_sensitivity);
            let model = settings.nova_model.clone();
            let (ready_tx, ready_rx) = oneshot::channel();
            tokio::spawn(run_stream(StreamContext {
                client,
                model,
                grammar,
                input_rx,
                output_tx,
                closing: closing.clone(),
                output_generation: output_generation.clone(),
                ready: ready_tx,
            }));
            match tokio::time::timeout(std::time::Duration::from_secs(20), ready_rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(failure))) => return Err(failure),
                Ok(Err(_)) => {
                    return Err(failure(
                        protocol::VoiceErrorCode::ProviderUnavailable,
                        true,
                        "startup_channel_closed",
                    ));
                }
                Err(_) => {
                    return Err(failure(
                        protocol::VoiceErrorCode::ProviderUnavailable,
                        true,
                        "startup_timeout",
                    ));
                }
            }
            Ok(Box::new(AwsNovaSession {
                input_tx: Some(input_tx),
                output_rx,
                closing,
                output_generation,
            }) as Box<dyn NovaSession>)
        })
    }
}

struct AwsNovaSession {
    input_tx: Option<mpsc::Sender<NovaInput>>,
    output_rx: mpsc::Receiver<NovaOutput>,
    closing: Arc<AtomicBool>,
    output_generation: Arc<AtomicU64>,
}
impl NovaSession for AwsNovaSession {
    fn send(&mut self, input: NovaInput) -> Result<NovaSend, ProviderFailure> {
        let sender = self.input_tx.as_ref().ok_or_else(|| {
            failure(
                protocol::VoiceErrorCode::ProviderUnavailable,
                true,
                "closed",
            )
        })?;
        if let NovaInput::Interrupt { output_generation } = &input {
            self.output_generation
                .store(*output_generation, Ordering::Release);
        }
        match sender.try_send(input) {
            Ok(()) => Ok(NovaSend::Sent),
            Err(mpsc::error::TrySendError::Full(NovaInput::Audio16Khz(_))) => {
                Ok(NovaSend::DroppedAudio)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(failure(
                protocol::VoiceErrorCode::ProviderUnavailable,
                true,
                "control_backpressure",
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(failure(
                protocol::VoiceErrorCode::ProviderUnavailable,
                true,
                "closed",
            )),
        }
    }
    fn output(&mut self) -> &mut mpsc::Receiver<NovaOutput> {
        &mut self.output_rx
    }
    fn close(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.input_tx.take();
    }
}

#[derive(Clone)]
struct Grammar {
    prompt: String,
    microphone: String,
    endpointing_sensitivity: settings_model::VoiceEndpointingSensitivity,
}
impl Grammar {
    fn new(
        _: &protocol::VoiceSessionId,
        endpointing_sensitivity: settings_model::VoiceEndpointingSensitivity,
    ) -> Self {
        Self {
            prompt: uuid::Uuid::new_v4().to_string(),
            microphone: uuid::Uuid::new_v4().to_string(),
            endpointing_sensitivity,
        }
    }
    fn opening(&self) -> Vec<serde_json::Value> {
        // Nova requires the prompt's FIRST content block to carry the SYSTEM
        // role; opening the microphone content first draws a
        // ValidationException ("First content must have SYSTEM role") that
        // kills every session.
        let system = uuid::Uuid::new_v4().to_string();
        vec![
            serde_json::json!({"event":{"sessionStart":{"inferenceConfiguration":{"maxTokens":1024,"topP":0.9,"temperature":0.7},"turnDetectionConfiguration":{"endpointingSensitivity":self.endpointing_sensitivity.nova_value()}}}}),
            serde_json::json!({"event":{"promptStart":{"promptName":self.prompt,"audioOutputConfiguration":{"mediaType":"audio/lpcm","sampleRateHertz":24000,"sampleSizeBits":16,"channelCount":1,"voiceId":"matthew","encoding":"base64","audioType":"SPEECH"},"textOutputConfiguration":{"mediaType":"text/plain"},"toolUseOutputConfiguration":{"mediaType":"application/json"},"toolConfiguration":{"tools":[{"toolSpec":{"name":TOOL_NAME,"description":"Send substantive work to the focused Tyde agent","inputSchema":{"json":"{\"type\":\"object\",\"properties\":{\"message\":{\"type\":\"string\"}},\"required\":[\"message\"]}"}}}]}}}}),
            serde_json::json!({"event":{"contentStart":{"promptName":self.prompt,"contentName":system,"role":"SYSTEM","type":"TEXT","interactive":false,"textInputConfiguration":{"mediaType":"text/plain"}}}}),
            serde_json::json!({"event":{"textInput":{"promptName":self.prompt,"contentName":system,"content":SYSTEM_PROMPT}}}),
            serde_json::json!({"event":{"contentEnd":{"promptName":self.prompt,"contentName":system}}}),
            serde_json::json!({"event":{"contentStart":{"promptName":self.prompt,"contentName":self.microphone,"role":"USER","type":"AUDIO","interactive":true,"audioInputConfiguration":{"mediaType":"audio/lpcm","sampleRateHertz":16000,"sampleSizeBits":16,"channelCount":1,"audioType":"SPEECH","encoding":"base64"}}}}),
        ]
    }
    fn encode(&self, input: NovaInput) -> Vec<serde_json::Value> {
        match input {
            NovaInput::Audio16Khz(samples) => {
                let mut bytes = Vec::with_capacity(samples.len() * 2);
                for sample in samples {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                vec![
                    serde_json::json!({"event":{"audioInput":{"promptName":self.prompt,"contentName":self.microphone,"content":base64::engine::general_purpose::STANDARD.encode(bytes)}}}),
                ]
            }
            NovaInput::Interrupt { .. } => Vec::new(),
            NovaInput::InputEnd => vec![
                serde_json::json!({"event":{"contentEnd":{"promptName":self.prompt,"contentName":self.microphone}}}),
            ],
            NovaInput::ToolResult {
                tool_use_id,
                message_id,
                result,
            } => {
                let content = uuid::Uuid::new_v4().to_string();
                vec![
                    serde_json::json!({"event":{"contentStart":{"promptName":self.prompt,"contentName":content,"role":"TOOL","type":"TOOL","interactive":false,"toolResultInputConfiguration":{"toolUseId":tool_use_id,"type":"TEXT","textInputConfiguration":{"mediaType":"text/plain"}}}}}),
                    serde_json::json!({"event":{"toolResult":{"promptName":self.prompt,"contentName":content,"content":serde_json::json!({"message_id":message_id,"result":result}).to_string()}}}),
                    serde_json::json!({"event":{"contentEnd":{"promptName":self.prompt,"contentName":content}}}),
                ]
            }
            // Nova permits exactly ONE SYSTEM content block per prompt, and
            // the opening grammar already sent it. Encoding progress as a
            // second SYSTEM block was a fatal ValidationException ("Duplicate
            // SYSTEM content") that killed every session on its first agent
            // call. Progress is surfaced through the UI transcript lane
            // instead of the provider prompt.
            NovaInput::Progress { .. } => Vec::new(),
            NovaInput::Stop => vec![
                serde_json::json!({"event":{"promptEnd":{"promptName":self.prompt}}}),
                serde_json::json!({"event":{"sessionEnd":{}}}),
            ],
        }
    }
}

struct InputStream {
    rx: Mutex<mpsc::Receiver<NovaInput>>,
    grammar: Grammar,
    opening: std::collections::VecDeque<serde_json::Value>,
}
impl FuturesStream for InputStream {
    type Item = Result<
        InvokeModelWithBidirectionalStreamInput,
        aws_sdk_bedrockruntime::types::error::InvokeModelWithBidirectionalStreamInputError,
    >;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let value = loop {
            if let Some(value) = this.opening.pop_front() {
                break value;
            }
            match this.rx.lock().expect("Nova input lock").poll_recv(cx) {
                Poll::Ready(Some(input)) => {
                    let mut values = this.grammar.encode(input);
                    if values.is_empty() {
                        continue;
                    }
                    if values.len() > 1 {
                        for value in values.drain(1..).rev() {
                            this.opening.push_front(value);
                        }
                    }
                    break values.remove(0);
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        };
        let Ok(bytes) = serde_json::to_vec(&value) else {
            return Poll::Ready(None);
        };
        let chunk = BidirectionalInputPayloadPart::builder()
            .bytes(Blob::new(bytes))
            .build();
        Poll::Ready(Some(Ok(InvokeModelWithBidirectionalStreamInput::Chunk(
            chunk,
        ))))
    }
}

struct StreamContext {
    client: aws_sdk_bedrockruntime::Client,
    model: String,
    grammar: Grammar,
    input_rx: mpsc::Receiver<NovaInput>,
    output_tx: mpsc::Sender<NovaOutput>,
    closing: Arc<AtomicBool>,
    output_generation: Arc<AtomicU64>,
    ready: oneshot::Sender<Result<(), ProviderFailure>>,
}

async fn run_stream(context: StreamContext) {
    use aws_sdk_bedrockruntime::primitives::event_stream::EventStreamSender;
    use aws_sdk_bedrockruntime::types::InvokeModelWithBidirectionalStreamOutput;
    let StreamContext {
        client,
        model,
        grammar,
        input_rx,
        output_tx,
        closing,
        output_generation,
        ready,
    } = context;
    let input = InputStream {
        rx: Mutex::new(input_rx),
        opening: grammar.opening().into(),
        grammar,
    };
    let response = client
        .invoke_model_with_bidirectional_stream()
        .model_id(model)
        .body(EventStreamSender::from(input))
        .send()
        .await;
    let mut response = match response {
        Ok(response) => {
            let _ = ready.send(Ok(()));
            response
        }
        Err(error) => {
            let detail = format!("{error:?}");
            let failure = categorize_start_failure(&detail);
            let _ = ready.send(Err(failure.clone()));
            if !closing.load(Ordering::Acquire) {
                tracing::warn!(
                    category = failure.category,
                    provider_error = %detail,
                    "Nova stream failed before acceptance"
                );
            }
            return;
        }
    };
    let mut parser = Parser {
        output_generation,
        ..Default::default()
    };
    loop {
        let event = match response.body.recv().await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(error) => {
                let raw = format!("{error:?}");
                let failure = categorize_start_failure(&raw);
                // Credential failures stay typed-only so provider text never
                // leaks tokens or account diagnostics; everything else keeps
                // the human-readable service message so the UI can show WHY
                // the session died instead of a generic "unavailable".
                let detail = match failure.category {
                    "credentials_expired" | "credentials_unavailable" => None,
                    _ => extract_service_message(&raw),
                };
                if closing.load(Ordering::Acquire) {
                    tracing::debug!(provider_error=%raw,"Nova stream receive failed during close");
                } else {
                    tracing::warn!(category = failure.category, provider_error=%raw,"Nova stream receive failed mid-session");
                }
                let _ = output_tx
                    .send(NovaOutput::ProviderError {
                        code: failure.code,
                        retryable: failure.retryable,
                        detail,
                    })
                    .await;
                return;
            }
        };
        let InvokeModelWithBidirectionalStreamOutput::Chunk(chunk) = event else {
            continue;
        };
        let Some(bytes) = chunk.bytes else { continue };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes.as_ref()) else {
            continue;
        };
        for output in parser.parse(&value) {
            if output_tx.send(output).await.is_err() {
                return;
            }
        }
    }
    let _ = output_tx.send(NovaOutput::End { clean: true }).await;
}

/// Pull the human-readable service message out of an SDK error's debug
/// representation — e.g. the ValidationException text inside
/// `message: Some("RequestId=… : Error(s):\nError 1 : …")`.
fn extract_service_message(debug: &str) -> Option<String> {
    let marker = "message: Some(\"";
    let start = debug.find(marker)? + marker.len();
    let rest = &debug[start..];
    let end = rest.find("\")")?;
    let text = rest[..end].replace("\\n", " ").replace("\\\"", "\"");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut bounded = trimmed.to_string();
    if bounded.len() > 300 {
        let mut cut = 300;
        while !bounded.is_char_boundary(cut) {
            cut -= 1;
        }
        bounded.truncate(cut);
        bounded.push('…');
    }
    Some(bounded)
}

fn categorize_start_failure(redacted_debug: &str) -> ProviderFailure {
    let text = redacted_debug.to_ascii_lowercase();
    if text.contains("expiredtoken")
        || text.contains("expired token")
        || text.contains("credentialsexpired")
    {
        failure(
            protocol::VoiceErrorCode::CredentialsExpired,
            true,
            "credentials_expired",
        )
    } else if text.contains("credential")
        || text.contains("unauthorized")
        || text.contains("accessdenied")
    {
        failure(
            protocol::VoiceErrorCode::NotAvailable,
            false,
            "credentials_unavailable",
        )
    } else if text.contains("validation") || text.contains("model") {
        failure(
            protocol::VoiceErrorCode::ProviderUnavailable,
            false,
            "model_or_region_rejected",
        )
    } else {
        failure(
            protocol::VoiceErrorCode::ProviderUnavailable,
            true,
            "provider_start_failed",
        )
    }
}

#[derive(Default)]
struct Parser {
    text: std::collections::HashMap<String, (protocol::VoiceTranscriptSpeaker, String)>,
    audio_generation: std::collections::HashMap<String, u64>,
    completion_generation: std::collections::HashMap<String, u64>,
    output_generation: Arc<AtomicU64>,
}
impl Parser {
    fn parse(&mut self, value: &serde_json::Value) -> Vec<NovaOutput> {
        let Some(event) = value.get("event") else {
            return vec![];
        };
        if event.get("error").is_some()
            || event.get("internalServerException").is_some()
            || event.get("throttlingException").is_some()
        {
            tracing::warn!(provider_event = %event, "Nova stream reported an error event");
            let detail = ["error", "internalServerException", "throttlingException"]
                .iter()
                .find_map(|key| event.get(*key))
                .and_then(|body| body.get("message"))
                .and_then(|message| message.as_str())
                .filter(|message| !message.trim().is_empty())
                .map(|message| message.trim().to_string());
            return vec![NovaOutput::ProviderError {
                code: protocol::VoiceErrorCode::ProviderUnavailable,
                retryable: true,
                detail,
            }];
        }
        if let Some(start) = event.get("completionStart") {
            if let Some(id) = completion_id(start) {
                self.completion_generation
                    .insert(id.into(), self.output_generation.load(Ordering::Acquire));
            }
            return vec![];
        }
        if let Some(start) = event.get("contentStart") {
            if start.get("type").and_then(|v| v.as_str()) == Some("TEXT") {
                if let Some(id) = content_id(start) {
                    let speaker = if start.get("role").and_then(|v| v.as_str()) == Some("USER") {
                        protocol::VoiceTranscriptSpeaker::User
                    } else {
                        protocol::VoiceTranscriptSpeaker::Assistant
                    };
                    self.text.insert(id.into(), (speaker, String::new()));
                }
            } else if start.get("type").and_then(|v| v.as_str()) == Some("AUDIO")
                && let Some(id) = content_id(start)
            {
                let generation = completion_id(start)
                    .and_then(|completion| self.completion_generation.get(completion).copied())
                    .unwrap_or_else(|| self.output_generation.load(Ordering::Acquire));
                self.audio_generation.insert(id.into(), generation);
            }
            return vec![];
        }
        if let Some(audio) = event.get("audioOutput")
            && let Some(content) = audio.get("content").and_then(|v| v.as_str())
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(content)
            && bytes.len().is_multiple_of(2)
            && bytes.len() <= 64 * 1024
        {
            let generation = content_id(audio)
                .and_then(|id| self.audio_generation.get(id).copied())
                .unwrap_or_else(|| self.output_generation.load(Ordering::Acquire));
            return vec![NovaOutput::Audio24Khz {
                output_generation: generation,
                samples: bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|v| i16::from_le_bytes(*v))
                    .collect(),
            }];
        }
        if let Some(text) = event.get("textOutput")
            && let (Some(id), Some(content)) = (
                content_id(text),
                text.get("content")
                    .and_then(|v| v.as_str())
                    .filter(|v| v.len() <= 8192),
            )
            && let Some((speaker, last)) = self.text.get_mut(id)
        {
            *last = content.into();
            return vec![NovaOutput::Transcript {
                speaker: *speaker,
                text: content.into(),
                is_final: false,
            }];
        }
        if let Some(tool) = event.get("toolUse")
            && tool.get("toolName").and_then(|v| v.as_str()) == Some(TOOL_NAME)
            && let (Some(id), Some(content)) = (
                tool.get("toolUseId").and_then(|v| v.as_str()),
                tool.get("content").and_then(|v| v.as_str()),
            )
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
            && let Some(message) = value
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|v| v.len() <= 16 * 1024)
        {
            return vec![NovaOutput::ToolUse {
                tool_use_id: id.into(),
                model_turn_id: protocol::ModelTurnId(
                    tool.get("modelTurnId")
                        .and_then(|value| value.as_str())
                        .unwrap_or(id)
                        .to_owned(),
                ),
                message: message.into(),
            }];
        }
        if let Some(end) = event.get("contentEnd") {
            if end.get("stopReason").and_then(|v| v.as_str()) == Some("INTERRUPTED") {
                return vec![NovaOutput::State(protocol::VoiceSessionState::Interrupting)];
            }
            if let Some(id) = content_id(end)
                && let Some((speaker, text)) = self.text.remove(id)
                && !text.is_empty()
            {
                return vec![NovaOutput::Transcript {
                    speaker,
                    text,
                    is_final: true,
                }];
            }
        }
        if let Some(end) = event.get("completionEnd") {
            if let Some(id) = completion_id(end) {
                self.completion_generation.remove(id);
            }
            return vec![];
        }
        vec![]
    }
}

fn content_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("contentName")
        .or_else(|| value.get("contentId"))?
        .as_str()
        .filter(|id| id.len() <= 256)
}

fn completion_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("completionId")?
        .as_str()
        .filter(|id| id.len() <= 256)
}

fn failure(
    code: protocol::VoiceErrorCode,
    retryable: bool,
    category: &'static str,
) -> ProviderFailure {
    ProviderFailure {
        code,
        retryable,
        category,
    }
}
