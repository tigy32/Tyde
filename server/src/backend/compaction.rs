use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use protocol::{
    BackendKind, CompactionMethod, CompactionMetrics, CompactionObservationId,
    CompactionOperationId, CompactionStage, CompactionTrigger, SessionId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendCompactionCapability {
    pub coordinator: BackendCompactionCoordinator,
    pub availability: BackendCompactionAvailability,
    pub provider_version: Option<String>,
    pub protocol_version: Option<String>,
    pub confidence: Option<BackendCompactionProtocolConfidence>,
    pub reseat: BackendContextReseatSupport,
    pub evidence: BackendCompactionCapabilityEvidence,
}

impl BackendCompactionCapability {
    pub(crate) fn legacy_unavailable(reason: BackendCompactionUnavailableReason) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::LegacyReplacement,
            availability: BackendCompactionAvailability::Unavailable { reason },
            provider_version: None,
            protocol_version: None,
            confidence: None,
            reseat: BackendContextReseatSupport::Unsupported,
            evidence: BackendCompactionCapabilityEvidence::AdapterContract,
        }
    }

    pub(crate) fn context_unavailable(reason: BackendCompactionUnavailableReason) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::ContextOperation,
            availability: BackendCompactionAvailability::Unavailable { reason },
            provider_version: None,
            protocol_version: None,
            confidence: None,
            reseat: BackendContextReseatSupport::Unsupported,
            evidence: BackendCompactionCapabilityEvidence::AdapterContract,
        }
    }

    pub(crate) fn native(
        mechanism: BackendCompactionMechanism,
        provider_version: Option<String>,
        confidence: BackendCompactionProtocolConfidence,
        reseat: BackendContextReseatSupport,
        evidence: BackendCompactionCapabilityEvidence,
    ) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::ContextOperation,
            availability: BackendCompactionAvailability::Native { mechanism },
            provider_version,
            protocol_version: None,
            confidence: Some(confidence),
            reseat,
            evidence,
        }
    }

    pub(crate) fn unknown(
        reason: BackendCompactionUnknownReason,
        provider_version: Option<String>,
        evidence: BackendCompactionCapabilityEvidence,
    ) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::ContextOperation,
            availability: BackendCompactionAvailability::Unknown { reason },
            provider_version,
            protocol_version: None,
            confidence: None,
            reseat: BackendContextReseatSupport::Unsupported,
            evidence,
        }
    }
}

impl Default for BackendCompactionCapability {
    fn default() -> Self {
        Self::legacy_unavailable(BackendCompactionUnavailableReason::AdapterHasNoManualTransport)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionCoordinator {
    LegacyReplacement,
    ContextOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionAvailability {
    Native {
        mechanism: BackendCompactionMechanism,
    },
    AutomaticOnly {
        reason: BackendCompactionUnavailableReason,
    },
    Unavailable {
        reason: BackendCompactionUnavailableReason,
    },
    Unknown {
        reason: BackendCompactionUnknownReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionMechanism {
    InterceptedTextCommand,
    JsonRpcRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionProtocolConfidence {
    Verified,
    Compatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendContextReseatSupport {
    PreservedByNative,
    InjectAfterNative,
    IncludeInNativeRequest,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionUnavailableReason {
    ManualTriggerAbsent,
    AdapterHasNoManualTransport,
    VersionTooOld { required: String },
    VersionNotAllowlisted { tested: String },
    TranscriptNotAuthoritative,
    ProviderDisabledCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionUnknownReason {
    ProcessNotInitialized,
    VersionUnavailable,
    VersionUnparseable,
    CapabilityProbeFailed(String),
    ProtocolNotAllowlisted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionCapabilityEvidence {
    ClaudeInitializeCommand { name: String },
    CodexInitializeUserAgent { user_agent: String },
    HermesLocalGatewayProbe { version: String },
    AdapterContract,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BackendContinuationContext {
    pub required: Vec<BackendContinuationItem>,
    pub advisory: Vec<BackendContinuationItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BackendContinuationItem {
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BackendContextSeed {
    pub workspace_roots: Vec<String>,
    pub summary: String,
    pub continuation: BackendContinuationContext,
}

impl BackendContextSeed {
    pub(crate) fn render_hidden_bootstrap(&self) -> Result<String, BackendBindingPrepareError> {
        let summary = self.summary.trim();
        if summary.is_empty() {
            return Err(BackendBindingPrepareError::InvalidSeed {
                message: "compacted backend seed summary was empty".to_owned(),
            });
        }
        let required = render_seed_items(&self.continuation.required);
        let advisory = render_seed_items(&self.continuation.advisory);
        let mut sections = vec![
            "Restore this compacted working context. Do not call tools. Acknowledge only with READY."
                .to_owned(),
            format!("Compacted context:\n{summary}"),
        ];
        if let Some(required) = required {
            sections.push(format!("Required continuation state:\n{required}"));
        }
        if let Some(advisory) = advisory {
            sections.push(format!("Advisory continuation state:\n{advisory}"));
        }
        Ok(sections.join("\n\n"))
    }

    pub(crate) fn continuation_result(&self) -> BackendContextReseatResult {
        BackendContextReseatResult {
            required: seed_install_status(&self.continuation.required),
            advisory: seed_install_status(&self.continuation.advisory),
        }
    }
}

fn render_seed_items(items: &[BackendContinuationItem]) -> Option<String> {
    (!items.is_empty()).then(|| {
        items
            .iter()
            .map(|item| format!("{}={}", item.kind, item.payload))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn seed_install_status(items: &[BackendContinuationItem]) -> ContinuationInstallStatus {
    if items.is_empty() {
        ContinuationInstallStatus::NotRequired
    } else {
        ContinuationInstallStatus::Installed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendBindingReadyEvidence {
    pub backend_kind: BackendKind,
    pub provider_session_id: SessionId,
    pub bootstrap_terminal_seen: bool,
    pub provider_idle_seen: bool,
    pub replay_or_setup_drained: bool,
    pub unsafe_activity_observed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendBindingPrepareError {
    InvalidSeed { message: String },
    SpawnFailed {
        backend_kind: BackendKind,
        message: String,
    },
    ResumeFailed {
        backend_kind: BackendKind,
        provider_session_id: SessionId,
        message: String,
    },
    ProviderIdentityMissing { backend_kind: BackendKind },
    ProviderIdentityChanged {
        backend_kind: BackendKind,
        before: SessionId,
        after: SessionId,
    },
    BootstrapFailed {
        backend_kind: BackendKind,
        message: String,
    },
    BootstrapUnsafeActivity {
        backend_kind: BackendKind,
        activity: String,
    },
    BootstrapStreamClosed { backend_kind: BackendKind },
    BootstrapTimedOut { backend_kind: BackendKind },
}

impl std::fmt::Display for BackendBindingPrepareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSeed { message } => formatter.write_str(message),
            Self::SpawnFailed {
                backend_kind,
                message,
            } => write!(
                formatter,
                "failed to spawn prepared {backend_kind:?} binding: {message}"
            ),
            Self::ResumeFailed {
                backend_kind,
                provider_session_id,
                message,
            } => write!(
                formatter,
                "failed to reopen prepared {backend_kind:?} binding {}: {message}",
                provider_session_id.0
            ),
            Self::ProviderIdentityMissing { backend_kind } => {
                write!(formatter, "prepared {backend_kind:?} binding had no provider identity")
            }
            Self::ProviderIdentityChanged {
                backend_kind,
                before,
                after,
            } => write!(
                formatter,
                "prepared {backend_kind:?} binding changed provider identity from {} to {}",
                before.0, after.0
            ),
            Self::BootstrapFailed {
                backend_kind,
                message,
            } => write!(
                formatter,
                "prepared {backend_kind:?} bootstrap failed: {message}"
            ),
            Self::BootstrapUnsafeActivity {
                backend_kind,
                activity,
            } => write!(
                formatter,
                "prepared {backend_kind:?} bootstrap attempted unsafe activity: {activity}"
            ),
            Self::BootstrapStreamClosed { backend_kind } => write!(
                formatter,
                "prepared {backend_kind:?} bootstrap stream closed before idle"
            ),
            Self::BootstrapTimedOut { backend_kind } => {
                write!(formatter, "prepared {backend_kind:?} bootstrap timed out")
            }
        }
    }
}

impl std::error::Error for BackendBindingPrepareError {}

#[derive(Debug, Clone)]
pub(crate) struct BackendCompactionRequest {
    pub operation_id: CompactionOperationId,
    pub trigger: CompactionTrigger,
    pub focus: Option<String>,
    pub transcript_authoritative: bool,
    pub continuation: BackendContinuationContext,
}

#[derive(Debug)]
pub(crate) enum BackendCompactionStart {
    Accepted(BackendAcceptedCompaction),
    Deferred {
        reason: BackendCompactionDeferredReason,
    },
    NotDispatched {
        reason: BackendCompactionNotDispatchedReason,
        fallback_safe: bool,
    },
    DispatchUncertain(BackendCompactionResult),
}

#[derive(Debug)]
pub(crate) struct BackendAcceptedCompaction {
    pub operation_id: CompactionOperationId,
    pub terminal: oneshot::Receiver<BackendCompactionResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionDeferredReason {
    ActiveTurn,
    ToolLifecycleActive,
    ApprovalPending,
    BackgroundMutationActive,
    AnotherCompactionActive,
    SessionInitializing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionNotDispatchedReason {
    NativeUnavailable(BackendCompactionUnavailableReason),
    CapabilityUnknown(BackendCompactionUnknownReason),
    BackendClosed,
    InvalidFocus,
    RequiredContinuationUnsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionDispatchState {
    Accepted,
    MayHaveReachedProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionMutationState {
    NotObserved,
    Completed,
    MayHaveMutated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PostCompactionTokenCount {
    Trusted(u64),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendCompactionResult {
    pub operation_id: CompactionOperationId,
    pub dispatch: BackendCompactionDispatchState,
    pub mutation: BackendCompactionMutationState,
    pub outcome: Result<BackendCompactionSuccess, BackendCompactionFailure>,
    pub provider_session_id: Option<SessionId>,
    pub metrics: CompactionMetrics,
    pub post_context_tokens: PostCompactionTokenCount,
    pub evidence: BackendCompactionTerminalEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendCompactionSuccess {
    pub mechanism: CompactionMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendCompactionFailure {
    pub kind: BackendCompactionFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionFailureKind {
    ProviderRejected,
    ProviderFailed,
    Interrupted,
    TimedOut,
    TransportClosed,
    ProtocolViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionTerminalEvidence {
    Claude {
        session_id: Option<String>,
        boundary_uuid: Option<String>,
        compact_result: Option<String>,
        terminal_result_seen: bool,
    },
    Codex {
        thread_id: String,
        turn_id: Option<String>,
        item_id: Option<String>,
        deprecated_notification_seen: bool,
    },
    Hermes {
        live_session_id: String,
        stored_session_id: String,
        response_status: Option<String>,
        rpc_code: Option<i64>,
    },
    DispatchUncertain,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionEvent {
    Progress(BackendCompactionProgress),
    Observed(BackendObservedCompaction),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendCompactionProgress {
    pub operation_id: CompactionOperationId,
    pub stage: CompactionStage,
    pub elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendObservedCompaction {
    pub observation_id: CompactionObservationId,
    pub trigger: CompactionTrigger,
    pub method: CompactionMethod,
    pub provider_session_id: Option<SessionId>,
    pub metrics: CompactionMetrics,
    pub source: BackendCompactionObservationSource,
    pub user_focus: Option<BackendCompactionUserFocus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionObservationSource {
    ClaudeBoundary { boundary_uuid: String },
    CodexItem {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    HermesEvent { event_id: String },
    MockEvent { event_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendCompactionUserFocus {
    pub text: String,
    pub provenance: BackendCompactionUserFocusProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BackendCompactionUserFocusProvenance {
    TydeRequest,
    ProviderEcho,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ContinuationInstallStatus {
    NotRequired,
    PreservedByNative,
    Installed,
    Unsupported,
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendContextReseatResult {
    pub required: ContinuationInstallStatus,
    pub advisory: ContinuationInstallStatus,
}

pub(crate) fn not_dispatched_for_capability(
    capability: &BackendCompactionCapability,
) -> Option<BackendCompactionStart> {
    match &capability.availability {
        BackendCompactionAvailability::Native { .. } => None,
        BackendCompactionAvailability::AutomaticOnly { reason }
        | BackendCompactionAvailability::Unavailable { reason } => {
            Some(BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::NativeUnavailable(reason.clone()),
                fallback_safe: true,
            })
        }
        BackendCompactionAvailability::Unknown { reason } => {
            Some(BackendCompactionStart::NotDispatched {
                reason: BackendCompactionNotDispatchedReason::CapabilityUnknown(reason.clone()),
                fallback_safe: false,
            })
        }
    }
}

pub(crate) fn stable_observation_id(
    backend: &str,
    provider_session_id: &str,
    provider_event_id: &str,
) -> CompactionObservationId {
    CompactionObservationId(format!(
        "{backend}:{provider_session_id}:{provider_event_id}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_capability_never_sets_fallback_safe() {
        let capability = BackendCompactionCapability::unknown(
            BackendCompactionUnknownReason::ProcessNotInitialized,
            None,
            BackendCompactionCapabilityEvidence::None,
        );
        assert!(matches!(
            not_dispatched_for_capability(&capability),
            Some(BackendCompactionStart::NotDispatched {
                fallback_safe: false,
                ..
            })
        ));
    }

    #[test]
    fn affirmative_unavailable_capability_sets_fallback_safe_before_dispatch() {
        let capability = BackendCompactionCapability::context_unavailable(
            BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
        );
        assert!(matches!(
            not_dispatched_for_capability(&capability),
            Some(BackendCompactionStart::NotDispatched {
                fallback_safe: true,
                ..
            })
        ));
    }

    #[test]
    fn hidden_seed_renders_summary_and_continuation_without_user_origin_metadata() {
        let seed = BackendContextSeed {
            workspace_roots: vec!["/tmp/workspace".to_owned()],
            summary: "Keep cursor `opaque:00/+==` verbatim.".to_owned(),
            continuation: BackendContinuationContext {
                required: vec![BackendContinuationItem {
                    kind: "plan".to_owned(),
                    payload: serde_json::json!({"cursor": "opaque:00/+=="}),
                }],
                advisory: Vec::new(),
            },
        };
        let rendered = seed.render_hidden_bootstrap().expect("render seed");
        assert!(rendered.contains("Keep cursor `opaque:00/+==` verbatim."));
        assert!(rendered.contains(r#"plan={"cursor":"opaque:00/+=="}"#));
        assert_eq!(
            seed.continuation_result(),
            BackendContextReseatResult {
                required: ContinuationInstallStatus::Installed,
                advisory: ContinuationInstallStatus::NotRequired,
            }
        );
    }
}
