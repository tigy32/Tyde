//! Backend contracts shared by Tyde agent integrations.
//!
//! This crate owns the interface between the host and agent backends. Concrete
//! backend implementations and Tyde server orchestration live outside it.

use std::collections::BTreeSet;

mod certification;
mod coverage;

pub use certification::{CertificationCase, CertificationTier};
pub use coverage::{
    ActivityCondition, AgentControlAuthorizationSet, AgentControlBoundaryRace,
    AgentControlCoverageCell, AgentControlCoverageLedger, AgentControlRequestMultiplicity,
    AgentControlTargetRelation, AmbientActivityOutcome, AmbientWorkKind,
    AssistantToolRepresentationCell, BackendNativeConfigDiscoveryCell, BackendSetupDiscoveryCell,
    CapacityLifecycleCell, ConformanceCoverageCell, ConformanceCoverageError,
    ConformanceCoverageLedger, ContractOutcome, ContractState, ContractStimulus,
    DynamicSessionDiscoveryCell, EnumeratedCoverageLedger, GenericToolBoundaryTiming,
    GenericToolCallRelation, GenericToolClientTopology, GenericToolContract,
    GenericToolLifecycleApplicability, GenericToolLifecycleBoundary, GenericToolLifecycleCell,
    GenericToolLifecyclePhase, GenericToolMultiplicity, ImageAttachmentActivityCell,
    ImageAttachmentCell, ImageCoverageApplicability, ImageLifecycleActivityCell,
    ImageLifecycleCell, InputAdmissionAction, InputAdmissionCoverageCell,
    InputAdmissionCoverageLedger, InputAdmissionKind, InputAdmissionState,
    InteractionConcurrencyApplicability, InteractionConcurrencyCell,
    InteractionConcurrencyContract, InteractionConcurrencyScenario, InteractionImageActivityCell,
    InteractionImagePayloadCell, LiveCustomizationCell, McpActivityCoverageCell,
    McpConfigurationRaceCell, McpConnectionOwnershipCell, McpFailureConsistencyCell,
    McpHttpTransportCell, MessageMetadataApplicabilityCell, MessageMetadataCoverageApplicability,
    MessageMetadataCoverageCell, NormalizedTurnCoverageCell, NormalizedTurnCoverageLedger,
    NormalizedTurnShape, OrchestrationCorrelationCell, OrchestrationLifecycleCell,
    OrchestrationMetadataCell, OrchestrationOutcomeCell, OrchestrationPayloadCell,
    OrchestrationPhaseCell, PlainTerminationMechanism, PlainTerminationPhase, ProgressCoverageCell,
    ProgressFamily, ProgressSubagentMode, ProgressTransition, ReasoningCoverageCell,
    RequestUsageLifecycleCell, RetryBoundary, RetryFailureSource, RetryLifecycleApplicability,
    RetryLifecycleCell, SessionListLifecycleCell, SkillLifecycleCell, SpecialToolContract,
    TaskUpdateLifecycleApplicability, TaskUpdateLifecycleCell, ToolContractClass, ToolInputVariant,
    ToolResultVariant, TraceInvariant, UsageLifecycleCell,
};

/// A normalized adapter observation consumed by live conformance tests.
#[derive(Debug, Clone)]
pub enum BackendObservation {
    Chat(protocol::ChatEvent),
    ModelRequestTokenUsage(protocol::ModelRequestTokenUsage),
    Compaction(BackendCompactionObservation),
    Other,
}

/// A provider compaction lifecycle event exposed to adapter conformance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendCompactionObservation {
    Progress {
        operation_id: protocol::CompactionOperationId,
        stage: protocol::CompactionStage,
        elapsed_ms: Option<u64>,
    },
    Observed {
        observation_id: protocol::CompactionObservationId,
        trigger: protocol::CompactionTrigger,
        method: protocol::CompactionMethod,
        provider_session_id: Option<protocol::SessionId>,
        metrics: protocol::CompactionMetrics,
    },
}

/// A behavior that a live backend session promises to support.
macro_rules! exhaustive_capabilities {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum BackendCapability {
            $($variant),+
        }

        impl BackendCapability {
            pub const ALL: [Self; 0usize $(+ exhaustive_capabilities!(@one $variant))+] = [
                $(Self::$variant),+
            ];
        }
    };
    (@one $variant:ident) => { 1usize };
}

exhaustive_capabilities! {
    ListSessions,
    ResumeSession,
    ForkSession,
    ImageInput,
    Interrupt,
    SessionSettings,
    StartupMcpServers,
    AgentControlTools,
    TurnUsageReported,
    // Reports a session-cumulative total alongside the turn. Split out from
    // TurnUsageReported because the two are different claims and the
    // assertions that need them differ: Tycode and Kiro report a turn and no
    // running total, so gating the running-total checks on TurnUsageReported
    // skipped them on data rather than on a declaration. A resumed session that
    // attached mid-conversation still reports Unavailable here -- session
    // state, not a retraction of the capability.
    CumulativeUsageReported,
    ModelRequestUsageReported,
    ContextUsageReported,
    ContextBreakdownReported,
    CompactionReported,
    Subagents,
    ForegroundSubagents,
    BackgroundSubagents,
    BackgroundTasks,
    // The runtime can stop one named background command without touching the
    // turn or any other background work, so the user gets a cancel affordance
    // on that card. Measured: Claude answers the `stop_task` control request
    // and reports the task stopped; Codex terminates the background terminal.
    CancelsBackgroundTasks,
    // The runtime hands a long-running *foreground* command back to the model
    // before it finishes, so continuing to run it is itself a tool call.
    // Measured on codex-cli 0.146.0: one turn running a 25-second foreground
    // command made five tool calls (one exec_command, four write_stdin) where a
    // blocking runtime makes one. Distinct from BackgroundTasks — Claude and
    // Hermes background work and check on it, but a foreground command blocks
    // them, so "continuing a command is a second action" asserts nothing there.
    YieldsRunningCommands,
    AgentInitiatedTurns,
    MidTurnSteering,
    ReasoningDeltas,
    TaskUpdates,
    TaskListReplacement,
    TaskListClear,
    WorkflowProgress,
    OpaqueToolProgress,
    RetryTelemetry,
    OrchestrationEvents,
    UserQuestionRequests,
    PlanApprovalRequests,
    WorkspaceInstructions,
    Customization,
    GenericModifyFile,
    GenericReadFiles,
    GenericSearchTypes,
    GenericGetTypeDocs,
    GenericGenerateImage,
    GenericWebSearch,
    GenericViewImage,
    GenericSleep,
    GenericOtherTool,
    CapacityTelemetry,
}

/// The capabilities declared by a live backend session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    capabilities: BTreeSet<BackendCapability>,
}

impl BackendCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = BackendCapability>) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub fn contains(&self, capability: BackendCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = BackendCapability> + '_ {
        self.capabilities.iter().copied()
    }

    pub fn validate(&self) -> Result<(), CapabilityDeclarationError> {
        self.require(
            BackendCapability::ContextBreakdownReported,
            BackendCapability::ContextUsageReported,
        )?;
        self.require(
            BackendCapability::ModelRequestUsageReported,
            BackendCapability::TurnUsageReported,
        )?;
        self.require(
            BackendCapability::CumulativeUsageReported,
            BackendCapability::TurnUsageReported,
        )?;
        self.require(
            BackendCapability::AgentControlTools,
            BackendCapability::StartupMcpServers,
        )?;
        self.require(
            BackendCapability::StartupMcpServers,
            BackendCapability::GenericOtherTool,
        )?;
        self.require(
            BackendCapability::ForegroundSubagents,
            BackendCapability::Subagents,
        )?;
        self.require(
            BackendCapability::BackgroundSubagents,
            BackendCapability::Subagents,
        )?;
        self.require(
            BackendCapability::BackgroundSubagents,
            BackendCapability::BackgroundTasks,
        )?;
        self.require(
            BackendCapability::CancelsBackgroundTasks,
            BackendCapability::BackgroundTasks,
        )?;
        Ok(())
    }

    fn require(
        &self,
        capability: BackendCapability,
        required: BackendCapability,
    ) -> Result<(), CapabilityDeclarationError> {
        if self.contains(capability) && !self.contains(required) {
            return Err(CapabilityDeclarationError {
                capability,
                required,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityDeclarationError {
    pub capability: BackendCapability,
    pub required: BackendCapability,
}

impl std::fmt::Display for CapabilityDeclarationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "capability {:?} requires {:?}",
            self.capability, self.required
        )
    }
}

impl std::error::Error for CapabilityDeclarationError {}

impl<const N: usize> From<[BackendCapability; N]> for BackendCapabilities {
    fn from(capabilities: [BackendCapability; N]) -> Self {
        Self::new(capabilities)
    }
}
