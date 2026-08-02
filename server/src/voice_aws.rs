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

use aws_sdk_bedrockruntime::error::SdkError;
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
    progress: Arc<Mutex<OutboundProgress>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutboundProgress {
    ordinal: u64,
    event_name: &'static str,
    encoded_bytes: usize,
}

impl Default for OutboundProgress {
    fn default() -> Self {
        Self {
            ordinal: 0,
            event_name: "none",
            encoded_bytes: 0,
        }
    }
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
                if let Ok(mut progress) = self.progress.lock() {
                    progress.ordinal = progress.ordinal.saturating_add(1);
                    progress.event_name = event.safe_event_name();
                    progress.encoded_bytes = bytes.len();
                }
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

    let outbound_progress = Arc::new(Mutex::new(OutboundProgress::default()));
    let input = NovaInputStream {
        receiver: Mutex::new(input_rx),
        progress: Arc::clone(&outbound_progress),
    };
    let body = EventStreamSender::from(input);
    let response = client
        .invoke_model_with_bidirectional_stream()
        .model_id(NOVA_MODEL_ID)
        .body(body)
        .send()
        .await;
    let response = match response {
        Ok(response) => {
            let initial_request_id = safe_request_id(response.request_id());
            let _ = ready_tx.send(Ok(()));
            (response, initial_request_id)
        }
        Err(error) => {
            if closing.load(Ordering::Acquire) {
                return;
            }
            let startup_request_id = safe_request_id(error.request_id());
            let classified = classify_sdk_error(&error, startup_request_id.as_deref());
            let progress = outbound_progress_snapshot(&outbound_progress);
            tracing::warn!(
                stage = "startup",
                category = %classified.failure.category,
                sdk_variant = classified.sdk_variant,
                service_code = classified.service_code,
                message_fingerprint = classified.message_fingerprint,
                dispatch_kind = classified.dispatch_kind,
                request_id = classified.failure.request_id.as_deref().unwrap_or("unavailable"),
                outbound_event_ordinal = progress.ordinal,
                outbound_event_name = progress.event_name,
                outbound_event_bytes = progress.encoded_bytes,
                "Nova bidirectional stream failed"
            );
            let _ = ready_tx.send(Err(classified.failure));
            return;
        }
    };
    let initial_request_id = response.1;
    let mut response = response.0;

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
                    request_id: initial_request_id.clone(),
                };
                let progress = outbound_progress_snapshot(&outbound_progress);
                tracing::warn!(
                    stage = "runtime",
                    category = %failure.category,
                    request_id = failure.request_id.as_deref().unwrap_or("unavailable"),
                    outbound_event_ordinal = progress.ordinal,
                    outbound_event_name = progress.event_name,
                    outbound_event_bytes = progress.encoded_bytes,
                    "Nova bidirectional stream closed unexpectedly"
                );
                let _ = output_tx
                    .send(NovaOutputEvent::ProviderFailed(failure))
                    .await;
                return;
            }
            Err(error) => {
                if closing.load(Ordering::Acquire) {
                    return;
                }
                let classified = classify_sdk_error(&error, initial_request_id.as_deref());
                let progress = outbound_progress_snapshot(&outbound_progress);
                tracing::warn!(
                    stage = "runtime",
                    category = %classified.failure.category,
                    sdk_variant = classified.sdk_variant,
                    service_code = classified.service_code,
                    message_fingerprint = classified.message_fingerprint,
                    dispatch_kind = classified.dispatch_kind,
                    request_id = classified.failure.request_id.as_deref().unwrap_or("unavailable"),
                    outbound_event_ordinal = progress.ordinal,
                    outbound_event_name = progress.event_name,
                    outbound_event_bytes = progress.encoded_bytes,
                    "Nova bidirectional stream failed"
                );
                let _ = output_tx
                    .send(NovaOutputEvent::ProviderFailed(classified.failure))
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
                request_id: initial_request_id.clone(),
            };
            let progress = outbound_progress_snapshot(&outbound_progress);
            tracing::warn!(
                stage = "runtime",
                category = %failure.category,
                request_id = failure.request_id.as_deref().unwrap_or("unavailable"),
                outbound_event_ordinal = progress.ordinal,
                outbound_event_name = progress.event_name,
                outbound_event_bytes = progress.encoded_bytes,
                "Nova returned an invalid event payload"
            );
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

struct ClassifiedAwsFailure {
    failure: ProviderFailure,
    sdk_variant: &'static str,
    service_code: &'static str,
    message_fingerprint: &'static str,
    dispatch_kind: &'static str,
}

fn classify_sdk_error<E, R>(
    error: &SdkError<E, R>,
    fallback_request_id: Option<&str>,
) -> ClassifiedAwsFailure
where
    E: ProvideErrorMetadata,
{
    let (failure, sdk_variant, service_code, message_fingerprint, dispatch_kind, request_id) =
        match error {
            SdkError::ConstructionFailure(_) => (
                classify_aws_failure(None, "sdk_construction"),
                "construction",
                "none",
                "not_inspected",
                "none",
                None,
            ),
            SdkError::TimeoutError(_) => (
                classify_aws_failure(None, "timeout"),
                "timeout",
                "none",
                "not_inspected",
                "none",
                None,
            ),
            SdkError::DispatchFailure(dispatch) => {
                let (category, kind) = if dispatch.is_timeout() {
                    ("dispatch_timeout", "timeout")
                } else if dispatch.is_io() {
                    ("dispatch_io", "io")
                } else if dispatch.is_user() {
                    ("dispatch_user", "user")
                } else {
                    ("dispatch_other", "other")
                };
                (
                    classify_aws_failure(None, category),
                    "dispatch",
                    "none",
                    "not_inspected",
                    kind,
                    None,
                )
            }
            SdkError::ResponseError(_) => (
                classify_aws_failure(None, "response_decode"),
                "response",
                "none",
                "not_inspected",
                "none",
                None,
            ),
            SdkError::ServiceError(service) => {
                let code = service.err().code();
                let safe_code = known_service_code(code).unwrap_or("unmodeled");
                let message_fingerprint = if safe_code == "unmodeled" {
                    safe_service_message_fingerprint(service.err().message())
                } else {
                    "not_inspected"
                };
                let fallback_category = match message_fingerprint {
                    "parse_input_chunk" => "parse_input",
                    "input_validation" => "unmodeled_validation",
                    _ => "service_error",
                };
                (
                    classify_aws_failure(code, fallback_category),
                    "service",
                    safe_code,
                    message_fingerprint,
                    "none",
                    service.err().meta().request_id(),
                )
            }
            _ => (
                classify_aws_failure(None, "sdk_unknown_variant"),
                "unknown",
                "none",
                "not_inspected",
                "none",
                None,
            ),
        };
    let request_id = safe_request_id(request_id).or_else(|| safe_request_id(fallback_request_id));
    ClassifiedAwsFailure {
        failure: ProviderFailure {
            request_id,
            ..failure
        },
        sdk_variant,
        service_code,
        message_fingerprint,
        dispatch_kind,
    }
}

fn classify_aws_failure(
    aws_code: Option<&str>,
    fallback_category: &'static str,
) -> ProviderFailure {
    let (code, category) = match aws_code {
        Some("AccessDeniedException") => (protocol::VoiceErrorCode::NotAvailable, "access_denied"),
        Some("ResourceNotFoundException" | "ModelNotReadyException") => {
            (protocol::VoiceErrorCode::NotAvailable, "model_unavailable")
        }
        Some("ServiceQuotaExceededException" | "ThrottlingException") => {
            (protocol::VoiceErrorCode::ProviderUnavailable, "quota")
        }
        Some("ValidationException") => (protocol::VoiceErrorCode::NotAvailable, "invalid_request"),
        Some("ModelTimeoutException") => (protocol::VoiceErrorCode::TimedOut, "timeout"),
        Some(
            "ServiceUnavailableException"
            | "ModelStreamErrorException"
            | "InternalServerException"
            | "ModelErrorException",
        ) => (
            protocol::VoiceErrorCode::ProviderUnavailable,
            "service_unavailable",
        ),
        _ => match fallback_category {
            "timeout" | "dispatch_timeout" => {
                (protocol::VoiceErrorCode::TimedOut, fallback_category)
            }
            "sdk_construction" | "dispatch_user" => {
                (protocol::VoiceErrorCode::NotAvailable, fallback_category)
            }
            "dispatch_io"
            | "dispatch_other"
            | "response_decode"
            | "service_error"
            | "parse_input"
            | "unmodeled_validation" => (
                protocol::VoiceErrorCode::ProviderUnavailable,
                fallback_category,
            ),
            _ => (protocol::VoiceErrorCode::Internal, fallback_category),
        },
    };
    ProviderFailure {
        code,
        category: category.to_owned(),
        request_id: None,
    }
}

fn safe_service_message_fingerprint(message: Option<&str>) -> &'static str {
    let Some(message) = message else {
        return "unrecognized";
    };
    if has_safe_semantic_prefix(message, &["unable", "to", "parse", "input", "chunk"]) {
        "parse_input_chunk"
    } else if has_safe_semantic_prefix(message, &["input", "validation", "failed"])
        || has_safe_semantic_prefix(
            message,
            &[
                "the",
                "input",
                "fails",
                "to",
                "satisfy",
                "the",
                "constraints",
                "specified",
                "by",
                "amazon",
                "bedrock",
            ],
        )
    {
        "input_validation"
    } else {
        "unrecognized"
    }
}

fn has_safe_semantic_prefix(mut message: &str, words: &[&str]) -> bool {
    message = message.trim_start();
    for (index, word) in words.iter().enumerate() {
        let Some(prefix) = message.get(..word.len()) else {
            return false;
        };
        if !prefix.eq_ignore_ascii_case(word) {
            return false;
        }
        message = &message[word.len()..];
        if index + 1 < words.len() {
            let trimmed = message.trim_start();
            if trimmed.len() == message.len() {
                return false;
            }
            message = trimmed;
        }
    }
    message.is_empty()
        || message
            .chars()
            .next()
            .is_some_and(|character| character.is_whitespace() || ":;,.!([{—-".contains(character))
}

fn known_service_code(code: Option<&str>) -> Option<&'static str> {
    match code? {
        "AccessDeniedException" => Some("AccessDeniedException"),
        "ResourceNotFoundException" => Some("ResourceNotFoundException"),
        "ModelNotReadyException" => Some("ModelNotReadyException"),
        "ServiceQuotaExceededException" => Some("ServiceQuotaExceededException"),
        "ThrottlingException" => Some("ThrottlingException"),
        "ValidationException" => Some("ValidationException"),
        "ModelTimeoutException" => Some("ModelTimeoutException"),
        "ServiceUnavailableException" => Some("ServiceUnavailableException"),
        "ModelStreamErrorException" => Some("ModelStreamErrorException"),
        "InternalServerException" => Some("InternalServerException"),
        "ModelErrorException" => Some("ModelErrorException"),
        _ => None,
    }
}

fn safe_request_id(value: Option<&str>) -> Option<String> {
    let value = value?;
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'=')))
    .then(|| value.to_owned())
}

fn outbound_progress_snapshot(progress: &Mutex<OutboundProgress>) -> OutboundProgress {
    progress
        .lock()
        .map(|progress| *progress)
        .unwrap_or(OutboundProgress {
            ordinal: 0,
            event_name: "unavailable",
            encoded_bytes: 0,
        })
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
    use aws_sdk_bedrockruntime::types::error::InvokeModelWithBidirectionalStreamOutputError;
    use futures_util::StreamExt;

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
            classify_aws_failure(Some("AccessDeniedException"), "service_error"),
            ProviderFailure {
                code: protocol::VoiceErrorCode::NotAvailable,
                category: "access_denied".to_owned(),
                request_id: None,
            }
        );
        assert_eq!(
            classify_aws_failure(Some("ThrottlingException"), "service_error").category,
            "quota"
        );
        assert_eq!(
            classify_aws_failure(Some("ValidationException"), "service_error").category,
            "invalid_request"
        );
        assert_eq!(
            classify_aws_failure(None, "dispatch_io").category,
            "dispatch_io"
        );
    }

    #[test]
    fn sdk_variants_remain_distinct_without_exposing_sources() {
        let construction: SdkError<InvokeModelWithBidirectionalStreamOutputError, ()> =
            SdkError::construction_failure(std::io::Error::other("secret construction text"));
        let timeout: SdkError<InvokeModelWithBidirectionalStreamOutputError, ()> =
            SdkError::timeout_error(std::io::Error::other("secret timeout text"));
        let dispatch: SdkError<InvokeModelWithBidirectionalStreamOutputError, ()> =
            SdkError::dispatch_failure(aws_sdk_bedrockruntime::error::ConnectorError::io(
                Box::new(std::io::Error::other("secret connector text")),
            ));
        let response: SdkError<InvokeModelWithBidirectionalStreamOutputError, ()> =
            SdkError::response_error(std::io::Error::other("secret response text"), ());

        for (error, variant, category) in [
            (construction, "construction", "sdk_construction"),
            (timeout, "timeout", "timeout"),
            (dispatch, "dispatch", "dispatch_io"),
            (response, "response", "response_decode"),
        ] {
            let classified = classify_sdk_error(&error, None);
            assert_eq!(classified.sdk_variant, variant);
            assert_eq!(classified.failure.category, category);
            assert!(!classified.failure.category.contains("secret"));
        }
    }

    #[test]
    fn unmodeled_service_metadata_is_not_copied() {
        let service = InvokeModelWithBidirectionalStreamOutputError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("ProviderEchoedPrompt")
                .message("SENSITIVE_TEST_SENTINEL")
                .custom("aws_request_id", "safe-request_123=")
                .build(),
        );
        let error = SdkError::service_error(service, ());
        let classified = classify_sdk_error(&error, None);
        assert_eq!(classified.sdk_variant, "service");
        assert_eq!(classified.service_code, "unmodeled");
        assert_eq!(classified.message_fingerprint, "unrecognized");
        assert_eq!(classified.failure.category, "service_error");
        for exposed in [
            classified.sdk_variant,
            classified.service_code,
            classified.message_fingerprint,
            classified.dispatch_kind,
            &classified.failure.category,
        ] {
            assert!(!exposed.contains("SENSITIVE_TEST_SENTINEL"));
        }
        assert_eq!(
            classified.failure.request_id.as_deref(),
            Some("safe-request_123=")
        );
    }

    #[test]
    fn service_message_fingerprints_are_closed_and_never_copy_input() {
        assert_eq!(
            safe_service_message_fingerprint(Some("Unable to parse input chunk")),
            "parse_input_chunk"
        );
        assert_eq!(
            safe_service_message_fingerprint(Some(
                "  INPUT   validation\nfailed: SENSITIVE_TEST_SENTINEL"
            )),
            "input_validation"
        );
        assert_eq!(
            safe_service_message_fingerprint(Some(
                "unable TO parse input chunk: SENSITIVE_TEST_SENTINEL"
            )),
            "parse_input_chunk"
        );
        for arbitrary in ["SENSITIVE_TEST_SENTINEL", "Unable to parse input chunks"] {
            assert_eq!(
                safe_service_message_fingerprint(Some(arbitrary)),
                "unrecognized"
            );
        }

        let service = InvokeModelWithBidirectionalStreamOutputError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .message("Unable to parse input chunk: SENSITIVE_TEST_SENTINEL")
                .build(),
        );
        let classified = classify_sdk_error(&SdkError::service_error(service, ()), None);
        assert_eq!(classified.message_fingerprint, "parse_input_chunk");
        assert_eq!(classified.failure.category, "parse_input");
        assert_eq!(
            classified.failure.code,
            protocol::VoiceErrorCode::ProviderUnavailable
        );
        assert!(classified.failure.request_id.is_none());
        for exposed in [
            classified.sdk_variant,
            classified.service_code,
            classified.message_fingerprint,
            classified.dispatch_kind,
            &classified.failure.category,
        ] {
            assert!(!exposed.contains("SENSITIVE_TEST_SENTINEL"));
        }
    }

    #[test]
    fn request_references_are_strictly_bounded_and_allowlisted() {
        assert_eq!(
            safe_request_id(Some("request-123_A=")).as_deref(),
            Some("request-123_A=")
        );
        assert!(safe_request_id(Some("request id")).is_none());
        assert!(safe_request_id(Some("request\nsecret")).is_none());
        assert!(safe_request_id(Some(&"a".repeat(129))).is_none());

        let runtime_timeout: SdkError<InvokeModelWithBidirectionalStreamOutputError, ()> =
            SdkError::timeout_error(std::io::Error::other("not retained"));
        assert_eq!(
            classify_sdk_error(&runtime_timeout, Some("initial-request-123"))
                .failure
                .request_id
                .as_deref(),
            Some("initial-request-123")
        );
    }

    #[tokio::test]
    async fn input_progress_records_only_allowlisted_shape_and_size() {
        let (tx, rx) = mpsc::channel(1);
        let progress = Arc::new(Mutex::new(OutboundProgress::default()));
        tx.send(NovaInputEvent::test_session_end())
            .await
            .expect("queue Nova event");
        drop(tx);
        let mut input = NovaInputStream {
            receiver: Mutex::new(rx),
            progress: Arc::clone(&progress),
        };
        let item = input
            .next()
            .await
            .expect("input event")
            .expect("valid input");
        let InvokeModelWithBidirectionalStreamInput::Chunk(chunk) = item else {
            panic!("expected Nova input chunk");
        };
        let encoded_bytes = chunk.bytes.expect("chunk bytes").as_ref().len();
        assert_eq!(
            outbound_progress_snapshot(&progress),
            OutboundProgress {
                ordinal: 1,
                event_name: "sessionEnd",
                encoded_bytes,
            }
        );
    }

    #[test]
    fn poisoned_progress_is_distinguishable_from_no_events() {
        let progress = Mutex::new(OutboundProgress::default());
        let _ = std::panic::catch_unwind(|| {
            let _guard = progress.lock().expect("progress lock");
            panic!("poison progress lock");
        });
        assert_eq!(
            outbound_progress_snapshot(&progress),
            OutboundProgress {
                ordinal: 0,
                event_name: "unavailable",
                encoded_bytes: 0,
            }
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
