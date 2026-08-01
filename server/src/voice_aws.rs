use base64::Engine;
use futures_util::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use aws_sdk_bedrockruntime::types::error::InvokeModelWithBidirectionalStreamInputError;
use aws_sdk_bedrockruntime::types::{
    BidirectionalInputPayloadPart, InvokeModelWithBidirectionalStreamInput,
};
use aws_smithy_types::Blob;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_types::request_id::RequestId;

use crate::voice::{
    NOVA_OUTPUT_QUEUE_CAPACITY, NovaInputEvent, NovaOpenFuture, NovaOutputEvent, NovaProvider,
    NovaReadyFuture, NovaSession, ProviderFailure, VoicePcmFrame, VoiceRuntimeError,
};
use protocol::{VoiceSettings, VoiceTranscriptSpeaker};

const NOVA_MODEL_ID: &str = "amazon.nova-2-sonic-v1:0";
const NOVA_INPUT_QUEUE_CAPACITY: usize = 128;
const MAX_NOVA_AUDIO_BASE64_BYTES: usize = 32 * 1024;
const MAX_NOVA_TEXT_BYTES: usize = 8 * 1024;
const MAX_NOVA_TOOL_INPUT_BYTES: usize = 64 * 1024;
const MAX_NOVA_OPEN_TEXT_BLOCKS: usize = 32;

pub(crate) struct AwsNovaProvider;

impl NovaProvider for AwsNovaProvider {
    fn open<'a>(&'a self, settings: &'a VoiceSettings) -> NovaOpenFuture<'a> {
        Box::pin(async move {
            let region = settings
                .aws_region
                .as_ref()
                .filter(|region| !region.trim().is_empty())
                .ok_or(VoiceRuntimeError::Unavailable)?;
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new(region.clone()));
            if let Some(profile) = settings
                .aws_profile
                .as_ref()
                .filter(|profile| !profile.trim().is_empty())
            {
                loader = loader.profile_name(profile);
            }
            let config = loader.load().await;
            let client = aws_sdk_bedrockruntime::Client::new(&config);
            let (input_tx, input_rx) = mpsc::channel(NOVA_INPUT_QUEUE_CAPACITY);
            let (output_tx, output_rx) = mpsc::channel(NOVA_OUTPUT_QUEUE_CAPACITY);
            let (ready_tx, ready_rx) = oneshot::channel();
            let closing = Arc::new(AtomicBool::new(false));
            tokio::spawn(run_stream(
                client,
                input_rx,
                output_tx,
                ready_tx,
                Arc::clone(&closing),
            ));
            Ok(Box::new(AwsNovaSession {
                input_tx: Some(input_tx),
                output_rx,
                ready_rx: Some(ready_rx),
                closing,
            }) as Box<dyn NovaSession>)
        })
    }
}

struct AwsNovaSession {
    input_tx: Option<mpsc::Sender<NovaInputEvent>>,
    output_rx: mpsc::Receiver<NovaOutputEvent>,
    ready_rx: Option<oneshot::Receiver<Result<(), ProviderFailure>>>,
    closing: Arc<AtomicBool>,
}

struct NovaInputStream {
    receiver: Mutex<mpsc::Receiver<NovaInputEvent>>,
}

impl Stream for NovaInputStream {
    type Item = Result<
        InvokeModelWithBidirectionalStreamInput,
        InvokeModelWithBidirectionalStreamInputError,
    >;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut receiver = self.receiver.lock().expect("Nova input mutex poisoned");
        match receiver.poll_recv(context) {
            Poll::Ready(Some(event)) => {
                let Ok(bytes) = serde_json::to_vec(&event) else {
                    return Poll::Ready(None);
                };
                let chunk = BidirectionalInputPayloadPart::builder()
                    .bytes(Blob::new(bytes))
                    .build();
                Poll::Ready(Some(Ok(InvokeModelWithBidirectionalStreamInput::Chunk(
                    chunk,
                ))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl NovaSession for AwsNovaSession {
    fn send(&mut self, event: NovaInputEvent) -> Result<(), VoiceRuntimeError> {
        if !crate::voice::valid_external_nova_event(&event) {
            return Err(VoiceRuntimeError::InvalidSignal);
        }
        self.input_tx
            .as_ref()
            .ok_or(VoiceRuntimeError::Closed)?
            .try_send(event)
            .map_err(|_| VoiceRuntimeError::Closed)
    }

    fn output(&mut self) -> &mut mpsc::Receiver<NovaOutputEvent> {
        &mut self.output_rx
    }

    fn ready(&mut self) -> NovaReadyFuture<'_> {
        Box::pin(async move {
            let receiver = self.ready_rx.take().ok_or(VoiceRuntimeError::Closed)?;
            receiver
                .await
                .map_err(|_| VoiceRuntimeError::Closed)?
                .map_err(VoiceRuntimeError::Provider)
        })
    }

    fn close(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.input_tx.take();
    }
}

async fn run_stream(
    client: aws_sdk_bedrockruntime::Client,
    input_rx: mpsc::Receiver<NovaInputEvent>,
    output_tx: mpsc::Sender<NovaOutputEvent>,
    ready_tx: oneshot::Sender<Result<(), ProviderFailure>>,
    closing: Arc<AtomicBool>,
) {
    use aws_sdk_bedrockruntime::types::InvokeModelWithBidirectionalStreamOutput;
    use aws_smithy_http::event_stream::EventStreamSender;

    let input = NovaInputStream {
        receiver: Mutex::new(input_rx),
    };
    let body = EventStreamSender::from(input);
    let response = client
        .invoke_model_with_bidirectional_stream()
        .model_id(NOVA_MODEL_ID)
        .body(body)
        .send()
        .await;
    let mut response = match response {
        Ok(response) => {
            let _ = ready_tx.send(Ok(()));
            response
        }
        Err(error) => {
            if closing.load(Ordering::Acquire) {
                return;
            }
            let service = error.as_service_error();
            let aws_code = service
                .and_then(ProvideErrorMetadata::code)
                .unwrap_or("SdkTransportError");
            let request_id = error.request_id().unwrap_or("unavailable");
            let failure = classify_aws_failure(aws_code);
            tracing::warn!(
                stage = "startup",
                category = %failure.category,
                aws_code,
                request_id,
                "Nova bidirectional stream failed"
            );
            let _ = ready_tx.send(Err(failure));
            return;
        }
    };

    let mut parser = NovaOutputParser::default();
    loop {
        let event = match response.body.recv().await {
            Ok(Some(event)) => event,
            Ok(None) => {
                if closing.load(Ordering::Acquire) {
                    return;
                }
                let failure = ProviderFailure {
                    code: protocol::VoiceErrorCode::ProviderUnavailable,
                    category: "stream_closed".to_owned(),
                };
                tracing::warn!(stage = "runtime", category = %failure.category, "Nova bidirectional stream closed unexpectedly");
                let _ = output_tx
                    .send(NovaOutputEvent::ProviderFailed(failure))
                    .await;
                return;
            }
            Err(error) => {
                if closing.load(Ordering::Acquire) {
                    return;
                }
                let service = error.as_service_error();
                let aws_code = service
                    .and_then(ProvideErrorMetadata::code)
                    .unwrap_or("EventStreamTransportError");
                let request_id = service
                    .map(ProvideErrorMetadata::meta)
                    .and_then(RequestId::request_id)
                    .unwrap_or("unavailable");
                let failure = classify_aws_failure(aws_code);
                tracing::warn!(
                    stage = "runtime",
                    category = %failure.category,
                    aws_code,
                    request_id,
                    "Nova bidirectional stream failed"
                );
                let _ = output_tx
                    .send(NovaOutputEvent::ProviderFailed(failure))
                    .await;
                return;
            }
        };
        let InvokeModelWithBidirectionalStreamOutput::Chunk(chunk) = event else {
            continue;
        };
        let Some(bytes) = chunk.bytes else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes.as_ref()) else {
            let failure = ProviderFailure {
                code: protocol::VoiceErrorCode::Internal,
                category: "invalid_response".to_owned(),
            };
            tracing::warn!(stage = "runtime", category = %failure.category, "Nova returned an invalid event payload");
            let _ = output_tx
                .send(NovaOutputEvent::ProviderFailed(failure))
                .await;
            return;
        };
        for output in parser.parse(&value) {
            if output_tx.send(output).await.is_err() {
                return;
            }
        }
    }
}

fn classify_aws_failure(aws_code: &str) -> ProviderFailure {
    let (code, category) = match aws_code {
        "AccessDeniedException" => (protocol::VoiceErrorCode::NotAvailable, "access_denied"),
        "ResourceNotFoundException" | "ModelNotReadyException" => {
            (protocol::VoiceErrorCode::NotAvailable, "model_unavailable")
        }
        "ServiceQuotaExceededException" | "ThrottlingException" => {
            (protocol::VoiceErrorCode::ProviderUnavailable, "quota")
        }
        "ValidationException" => (protocol::VoiceErrorCode::NotAvailable, "invalid_request"),
        "ModelTimeoutException" => (protocol::VoiceErrorCode::TimedOut, "timeout"),
        "ServiceUnavailableException"
        | "ModelStreamErrorException"
        | "InternalServerException"
        | "ModelErrorException" => (
            protocol::VoiceErrorCode::ProviderUnavailable,
            "service_unavailable",
        ),
        "SdkTransportError" => (
            protocol::VoiceErrorCode::NotAvailable,
            "credentials_or_transport",
        ),
        "EventStreamTransportError" => (
            protocol::VoiceErrorCode::ProviderUnavailable,
            "stream_transport",
        ),
        _ => (protocol::VoiceErrorCode::Internal, "internal"),
    };
    ProviderFailure {
        code,
        category: category.to_owned(),
    }
}

#[derive(Clone, Copy)]
struct TextBlock {
    speaker: VoiceTranscriptSpeaker,
    is_final: bool,
}

#[derive(Default)]
struct NovaOutputParser {
    text_blocks: HashMap<String, TextBlock>,
}

impl NovaOutputParser {
    fn parse(&mut self, value: &serde_json::Value) -> Vec<NovaOutputEvent> {
        let Some(event) = value.get("event") else {
            return Vec::new();
        };
        if let Some(start) = event.get("contentStart") {
            if start.get("type").and_then(serde_json::Value::as_str) == Some("TEXT") {
                let Some(content_id) = start.get("contentId").and_then(serde_json::Value::as_str)
                else {
                    return Vec::new();
                };
                if content_id.len() > 256 || self.text_blocks.len() >= MAX_NOVA_OPEN_TEXT_BLOCKS {
                    return Vec::new();
                }
                let speaker = match start.get("role").and_then(serde_json::Value::as_str) {
                    Some("USER") => VoiceTranscriptSpeaker::User,
                    _ => VoiceTranscriptSpeaker::Assistant,
                };
                let is_final = start
                    .get("additionalModelFields")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|fields| serde_json::from_str::<serde_json::Value>(fields).ok())
                    .and_then(|fields| {
                        fields
                            .get("generationStage")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some("FINAL");
                self.text_blocks
                    .insert(content_id.to_owned(), TextBlock { speaker, is_final });
            }
            return Vec::new();
        }
        if let Some(audio) = event.get("audioOutput") {
            let Some(content) = audio.get("content").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            if content.len() > MAX_NOVA_AUDIO_BASE64_BYTES {
                return Vec::new();
            }
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(content) else {
                return Vec::new();
            };
            if bytes.len() % 2 != 0 {
                return Vec::new();
            }
            let samples = bytes
                .chunks_exact(2)
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
                .collect();
            return vec![NovaOutputEvent::AudioOutput(VoicePcmFrame {
                sample_rate_hertz: 24_000,
                samples,
            })];
        }
        if let Some(tool) = event.get("toolUse") {
            let Some(tool_use_id) = tool.get("toolUseId").and_then(serde_json::Value::as_str)
            else {
                return Vec::new();
            };
            let Some(name) = tool.get("toolName").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            if tool_use_id.is_empty()
                || tool_use_id.len() > 256
                || name.is_empty()
                || name.len() > 128
            {
                return Vec::new();
            }
            let input = tool
                .get("content")
                .and_then(serde_json::Value::as_str)
                .filter(|content| content.len() <= MAX_NOVA_TOOL_INPUT_BYTES)
                .and_then(|content| serde_json::from_str(content).ok())
                .unwrap_or(serde_json::Value::Null);
            return vec![NovaOutputEvent::ToolUse {
                tool_use_id: tool_use_id.to_owned(),
                name: name.to_owned(),
                input,
            }];
        }
        if let Some(text) = event.get("textOutput") {
            let Some(content) = text.get("content").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            if content.len() > MAX_NOVA_TEXT_BYTES {
                return Vec::new();
            }
            let Some(content_id) = text.get("contentId").and_then(serde_json::Value::as_str) else {
                return Vec::new();
            };
            let Some(block) = self.text_blocks.get(content_id).copied() else {
                return Vec::new();
            };
            return vec![NovaOutputEvent::Transcript {
                speaker: block.speaker,
                text: content.to_owned(),
                is_final: block.is_final,
            }];
        }
        if let Some(end) = event.get("contentEnd") {
            if let Some(content_id) = end.get("contentId").and_then(serde_json::Value::as_str) {
                self.text_blocks.remove(content_id);
            }
            if end.get("stopReason").and_then(serde_json::Value::as_str) == Some("INTERRUPTED") {
                return vec![NovaOutputEvent::Interrupted {}];
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_external_tool_use_shape() {
        let parsed = NovaOutputParser::default().parse(&serde_json::json!({"event": {"toolUse": {
            "toolUseId": "tool-1",
            "toolName": "send_to_focused_tyde_agent",
            "content": "{\"message\":\"do it\"}"
        }}}));
        assert!(
            matches!(parsed.as_slice(), [NovaOutputEvent::ToolUse { tool_use_id, name, input }]
            if tool_use_id == "tool-1" && name == "send_to_focused_tyde_agent" && input["message"] == "do it")
        );
    }

    #[test]
    fn aws_failure_categories_are_actionable_and_redacted() {
        assert_eq!(
            classify_aws_failure("AccessDeniedException"),
            ProviderFailure {
                code: protocol::VoiceErrorCode::NotAvailable,
                category: "access_denied".to_owned(),
            }
        );
        assert_eq!(
            classify_aws_failure("ThrottlingException").category,
            "quota"
        );
        assert_eq!(
            classify_aws_failure("ValidationException").category,
            "invalid_request"
        );
        assert_eq!(
            classify_aws_failure("SdkTransportError").category,
            "credentials_or_transport"
        );
    }

    #[tokio::test]
    async fn aws_session_readiness_waits_for_stream_handshake() {
        let (input_tx, _input_rx) = mpsc::channel(1);
        let (_output_tx, output_rx) = mpsc::channel(1);
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut session = AwsNovaSession {
            input_tx: Some(input_tx),
            output_rx,
            ready_rx: Some(ready_rx),
            closing: Arc::new(AtomicBool::new(false)),
        };
        let mut ready = Box::pin(session.ready());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut ready)
                .await
                .is_err()
        );
        ready_tx.send(Ok(())).expect("signal stream handshake");
        ready.await.expect("stream ready");
    }

    #[test]
    fn text_output_uses_documented_content_start_metadata() {
        let mut parser = NovaOutputParser::default();
        assert!(
            parser
                .parse(&serde_json::json!({"event": {"contentStart": {
                    "contentId": "content-1",
                    "type": "TEXT",
                    "role": "USER",
                    "additionalModelFields": "{\"generationStage\":\"FINAL\"}"
                }}}))
                .is_empty()
        );
        let parsed = parser.parse(&serde_json::json!({"event": {"textOutput": {
            "contentId": "content-1",
            "content": "hello"
        }}}));
        assert!(matches!(parsed.as_slice(), [NovaOutputEvent::Transcript {
            speaker: VoiceTranscriptSpeaker::User,
            text,
            is_final: true,
        }] if text == "hello"));
    }
}
