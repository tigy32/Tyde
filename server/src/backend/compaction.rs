use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use protocol::{
    BackendKind, CompactionMethod, CompactionMetrics, CompactionObservationId,
    CompactionOperationId, CompactionStage, CompactionTrigger, SessionId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCompactionCapability {
    pub coordinator: BackendCompactionCoordinator,
    pub availability: BackendCompactionAvailability,
    pub provider_version: Option<String>,
    pub protocol_version: Option<String>,
    pub evidence: BackendCompactionCapabilityEvidence,
}

impl BackendCompactionCapability {
    pub(crate) fn legacy_unavailable(reason: BackendCompactionUnavailableReason) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::LegacyReplacement,
            availability: BackendCompactionAvailability::Unavailable { reason },
            provider_version: None,
            protocol_version: None,
            evidence: BackendCompactionCapabilityEvidence::AdapterContract,
        }
    }

    pub(crate) fn context_unavailable(reason: BackendCompactionUnavailableReason) -> Self {
        Self::context_unavailable_with_metadata(
            reason,
            None,
            BackendCompactionCapabilityEvidence::AdapterContract,
        )
    }

    pub(crate) fn context_unavailable_with_metadata(
        reason: BackendCompactionUnavailableReason,
        provider_version: Option<String>,
        evidence: BackendCompactionCapabilityEvidence,
    ) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::ContextOperation,
            availability: BackendCompactionAvailability::Unavailable { reason },
            provider_version,
            protocol_version: None,
            evidence,
        }
    }

    pub(crate) fn native(
        mechanism: BackendCompactionMechanism,
        provider_version: Option<String>,
        evidence: BackendCompactionCapabilityEvidence,
    ) -> Self {
        Self {
            coordinator: BackendCompactionCoordinator::ContextOperation,
            availability: BackendCompactionAvailability::Native { mechanism },
            provider_version,
            protocol_version: None,
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
pub enum BackendCompactionCoordinator {
    LegacyReplacement,
    ContextOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionAvailability {
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
pub enum BackendCompactionMechanism {
    InterceptedTextCommand,
    JsonRpcRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionUnavailableReason {
    ManualTriggerAbsent,
    AdapterHasNoManualTransport,
    TranscriptNotAuthoritative,
    ProviderDisabledCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionUnknownReason {
    ProcessNotInitialized,
    CapabilityProbeFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionCapabilityEvidence {
    ClaudeInitializeCommand { name: String },
    CodexMethodProbe,
    HermesMethodProbe,
    AdapterContract,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BackendContextSeed {
    pub workspace_roots: Vec<String>,
    pub summary: String,
}

impl BackendContextSeed {
    pub(crate) fn render_hidden_bootstrap(&self) -> Result<String, BackendBindingPrepareError> {
        let summary = self.summary.trim();
        if summary.is_empty() {
            return Err(BackendBindingPrepareError::InvalidSeed {
                message: "compacted backend seed summary was empty".to_owned(),
            });
        }
        let sections = [
            "Restore this compacted working context. Do not call tools. Acknowledge only with READY."
                .to_owned(),
            format!("Compacted context:\n{summary}"),
        ];
        Ok(sections.join("\n\n"))
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
    InvalidSeed {
        message: String,
    },
    SpawnFailed {
        backend_kind: BackendKind,
        message: String,
    },
    ResumeFailed {
        backend_kind: BackendKind,
        provider_session_id: SessionId,
        message: String,
    },
    ProviderIdentityMissing {
        backend_kind: BackendKind,
    },
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
    BootstrapStreamClosed {
        backend_kind: BackendKind,
    },
    BootstrapTimedOut {
        backend_kind: BackendKind,
    },
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
                write!(
                    formatter,
                    "prepared {backend_kind:?} binding had no provider identity"
                )
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
pub struct BackendCompactionRequest {
    pub operation_id: CompactionOperationId,
    pub trigger: CompactionTrigger,
    pub focus: Option<String>,
    pub transcript_authoritative: bool,
}

#[derive(Debug)]
pub enum BackendCompactionStart {
    Accepted(BackendAcceptedCompaction),
    Deferred {
        reason: BackendCompactionDeferredReason,
    },
    NotDispatched {
        reason: BackendCompactionNotDispatchedReason,
        fallback_safe: bool,
    },
    DispatchUncertain(Box<BackendCompactionResult>),
}

#[derive(Debug)]
pub struct BackendAcceptedCompaction {
    pub operation_id: CompactionOperationId,
    pub terminal: oneshot::Receiver<BackendCompactionResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionDeferredReason {
    ActiveTurn,
    ToolLifecycleActive,
    ApprovalPending,
    BackgroundMutationActive,
    AnotherCompactionActive,
    SessionInitializing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionNotDispatchedReason {
    NativeUnavailable(BackendCompactionUnavailableReason),
    CapabilityUnknown(BackendCompactionUnknownReason),
    BackendClosed,
    InvalidFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionDispatchState {
    Rejected,
    Accepted,
    MayHaveReachedProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionMutationState {
    NotObserved,
    Completed,
    MayHaveMutated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostCompactionTokenCount {
    Trusted(u64),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCompactionResult {
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
pub struct BackendCompactionSuccess {
    pub mechanism: CompactionMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCompactionFailure {
    pub kind: BackendCompactionFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionFailureKind {
    ProviderRejected,
    ProviderFailed,
    Interrupted,
    TimedOut,
    TransportClosed,
    ProtocolViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCompactionTerminalEvidence {
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
    Observed(Box<BackendObservedCompaction>),
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
    ClaudeBoundary {
        boundary_uuid: String,
    },
    CodexItem {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    HermesEvent {
        event_id: String,
    },
    MockEvent {
        event_id: String,
    },
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

    const UNKNOWN_REASON_COUNT: usize = 2;
    const UNAVAILABLE_REASON_COUNT: usize = 4;

    #[test]
    fn every_unknown_reason_stays_fail_closed() {
        fn reason_name(reason: &BackendCompactionUnknownReason) -> &'static str {
            match reason {
                BackendCompactionUnknownReason::ProcessNotInitialized => "process_not_initialized",
                BackendCompactionUnknownReason::CapabilityProbeFailed(_) => {
                    "capability_probe_failed"
                }
            }
        }
        let reasons: [BackendCompactionUnknownReason; UNKNOWN_REASON_COUNT] = [
            BackendCompactionUnknownReason::ProcessNotInitialized,
            BackendCompactionUnknownReason::CapabilityProbeFailed("probe".to_owned()),
        ];
        for reason in reasons {
            assert!(!reason_name(&reason).is_empty());
            let capability = BackendCompactionCapability::unknown(
                reason,
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
    }

    #[test]
    fn every_affirmative_unavailable_reason_is_safe_before_dispatch() {
        fn reason_name(reason: &BackendCompactionUnavailableReason) -> &'static str {
            match reason {
                BackendCompactionUnavailableReason::ManualTriggerAbsent => "manual_trigger_absent",
                BackendCompactionUnavailableReason::AdapterHasNoManualTransport => {
                    "adapter_has_no_manual_transport"
                }
                BackendCompactionUnavailableReason::TranscriptNotAuthoritative => {
                    "transcript_not_authoritative"
                }
                BackendCompactionUnavailableReason::ProviderDisabledCommand => {
                    "provider_disabled_command"
                }
            }
        }
        let reasons: [BackendCompactionUnavailableReason; UNAVAILABLE_REASON_COUNT] = [
            BackendCompactionUnavailableReason::ManualTriggerAbsent,
            BackendCompactionUnavailableReason::AdapterHasNoManualTransport,
            BackendCompactionUnavailableReason::TranscriptNotAuthoritative,
            BackendCompactionUnavailableReason::ProviderDisabledCommand,
        ];
        for reason in reasons {
            assert!(!reason_name(&reason).is_empty());
            let capability = BackendCompactionCapability::context_unavailable(reason);
            assert!(matches!(
                not_dispatched_for_capability(&capability),
                Some(BackendCompactionStart::NotDispatched {
                    fallback_safe: true,
                    ..
                })
            ));
        }
    }

    #[test]
    fn hidden_seed_renders_summary_without_user_origin_metadata() {
        let seed = BackendContextSeed {
            workspace_roots: vec!["/tmp/workspace".to_owned()],
            summary: "Keep cursor `opaque:00/+==` verbatim.".to_owned(),
        };
        let rendered = seed.render_hidden_bootstrap().expect("render seed");
        assert!(rendered.contains("Keep cursor `opaque:00/+==` verbatim."));
        assert!(rendered.starts_with(
            "Restore this compacted working context. Do not call tools. Acknowledge only with READY."
        ));
    }
}
