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

impl BackendCompactionTerminalEvidence {
    /// The id the backend will mint when it reports observing this same
    /// compaction, when the terminal result carries enough to name it.
    ///
    /// A requested compaction is sighted twice — once as this operation's
    /// terminal result, once as the backend's own observation — and the two
    /// travel different channels, so their arrival order is not fixed. Naming
    /// the observation here is what lets the second sighting be recognized as a
    /// duplicate rather than becoming a second row in the user's transcript.
    ///
    /// Hermes carries no per-event id, so it cannot name this correlation.
    pub(crate) fn observation_id(&self) -> Option<CompactionObservationId> {
        match self {
            Self::Claude {
                session_id: Some(session_id),
                boundary_uuid: Some(boundary_uuid),
                ..
            } => Some(stable_observation_id("claude", session_id, boundary_uuid)),
            Self::Codex {
                thread_id,
                turn_id: Some(turn_id),
                item_id: Some(item_id),
                ..
            } => Some(stable_observation_id(
                "codex",
                thread_id,
                &format!("{turn_id}:{item_id}"),
            )),
            _ => None,
        }
    }
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
    HermesRpc {
        operation_id: CompactionOperationId,
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
