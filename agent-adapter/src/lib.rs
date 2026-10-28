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
    SetWorkspaceRoots,
    SetMultipleWorkspaceRoots,
    ForkSession,
    ImageInput,
    Interrupt,
    SessionSettings,
    SessionSpeed,
    StartupMcpServers,
    // An image block in a successful MCP tool result reaches both the model,
    // as an image, and the completion, intact. Grok moves it into a synthetic
    // user message and Hermes into a cache file path, so neither stream carries
    // the pixels. Kiro reports the block but gives its model base64 text: the
    // model decoded the PNG with a shell command instead of seeing it.
    McpImageResults,
    // Retains the server's original text block beside its typed image in the
    // canonical result used by real_mcp_image_text_fidelity, not native
    // presentation text. This does not certify arbitrary block interleaving,
    // structuredContent, or _meta.
    McpOriginalContentFidelity,
    // An MCP result with `isError: true` completes as a failure carrying the
    // server's result. Antigravity and Hermes reach MCP servers through Tyde's
    // bridge, which reports every call as a success so the provider keeps the
    // embedded result, and both then flatten it to plain text.
    McpErrorResults,
    // An MCP failure also retains its original image pixels and media type
    // beside the server text, without losing the failed outcome. Error status
    // and text preservation alone do not promise failed-image retention.
    McpErrorImageResults,
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
    // Identity-bearing own-request totals survive resume/restart and exclude
    // inherited fork work. Session-cumulative/message usage alone is weaker.
    DurableOwnRequestUsage,
    // Native compaction performs metered requests and exposes their exact
    // request usage. CompactionReported alone promises no billing evidence.
    CompactionRequestUsageReported,
    ContextUsageReported,
    ContextBreakdownReported,
    CompactionReported,
    // Manual admission is separate from its command-correlated durable target
    // and outcome; any completed installation is attested on the same ledger.
    DurableCompactionLedger,
    Subagents,
    NativeSubagentHierarchy,
    ExcludeAgentDelegation,
    EnforcedReadOnly,
    // Exact allow and deny lists are enforced by the native tool catalogue.
    NativeToolPolicies,
    NativeSubagentWaitProgress,
    ForegroundSubagents,
    BackgroundSubagents,
    // Commands can outlive their launching turn. This does not promise that
    // interrupting a turn preserves them; cancellation scope is separate.
    BackgroundTasks,
    // Interrupting either the launching turn or a later foreground turn leaves
    // already-backgrounded commands running until their own terminal outcome.
    InterruptPreservesBackgroundTasks,
    // Background commands share their still-active owning turn's cancellation
    // scope. This says nothing about other turns, subagents, or workflows.
    InterruptCancelsOwnedBackgroundCommands,
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
    // Native JavaScript cells can yield to the model while their awaited
    // tools remain foreground work, then be collected with a native wait.
    YieldedCodeCells,
    // A foreground command that outlives its timeout is moved to the
    // background instead of killed, and the runtime starts a turn of its own
    // when it finishes. Measured on Claude Code 2.1.283: a Bash call with a
    // 3000 ms timeout reports `task_started {is_backgrounded: true}` and a
    // tool result naming the background task id. Codex yields an
    // `exec_command` at its `yield_time_ms` and keeps the process running.
    // Hermes kills a foreground command at its timeout.
    MovesTimedOutCommandsToBackground,
    NativeGoals,
    // The runtime advertises the slash commands its session accepts and runs
    // one when a user message starts with its `/name`. Measured: Claude lists
    // them in its `initialize` response and `init` frame; ACP agents send
    // `available_commands_update`. Codex app-server exposes no command list.
    SlashCommands,
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
    UserQuestionHistory,
    StructuredUserQuestionAnswers,
    AsyncUserQuestionRequests,
    PlanApprovalRequests,
    WorkspaceInstructions,
    Customization,
    // Selected native skills share a name namespace with filesystem-discovered
    // skills: equivalent complete content is usable without a warning; differing
    // content and unreadable trees are diagnosed rather than silently substituted.
    NativeSkillContentCollisions,
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
    // The backend can report account capacity without a conversation: a
    // short-lived provider process or connection answers a read-only status
    // request and exits. Zero model tokens, no turn, no session. This is what
    // makes host-owned polling possible; `CapacityTelemetry` alone only
    // promises a report while an agent happens to be running.
    OutOfBandCapacity,
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
            BackendCapability::DurableOwnRequestUsage,
            BackendCapability::ModelRequestUsageReported,
        )?;
        self.require(
            BackendCapability::DurableOwnRequestUsage,
            BackendCapability::CumulativeUsageReported,
        )?;
        self.require(
            BackendCapability::DurableOwnRequestUsage,
            BackendCapability::ResumeSession,
        )?;
        self.require(
            BackendCapability::CompactionRequestUsageReported,
            BackendCapability::ModelRequestUsageReported,
        )?;
        self.require(
            BackendCapability::CompactionRequestUsageReported,
            BackendCapability::CompactionReported,
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
            BackendCapability::McpErrorImageResults,
            BackendCapability::McpImageResults,
        )?;
        self.require(
            BackendCapability::McpErrorImageResults,
            BackendCapability::McpErrorResults,
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
        for scope in [
            BackendCapability::InterruptPreservesBackgroundTasks,
            BackendCapability::InterruptCancelsOwnedBackgroundCommands,
        ] {
            self.require(scope, BackendCapability::BackgroundTasks)?;
            self.require(scope, BackendCapability::Interrupt)?;
        }
        let preserves = self.contains(BackendCapability::InterruptPreservesBackgroundTasks);
        let cancels = self.contains(BackendCapability::InterruptCancelsOwnedBackgroundCommands);
        if preserves && cancels {
            return Err(CapabilityDeclarationError::Conflicting {
                first: BackendCapability::InterruptPreservesBackgroundTasks,
                second: BackendCapability::InterruptCancelsOwnedBackgroundCommands,
            });
        }
        if self.contains(BackendCapability::BackgroundTasks)
            && self.contains(BackendCapability::Interrupt)
            && !preserves
            && !cancels
        {
            return Err(CapabilityDeclarationError::MissingBackgroundInterruptionScope);
        }
        self.require(
            BackendCapability::MovesTimedOutCommandsToBackground,
            BackendCapability::BackgroundTasks,
        )?;
        self.require(
            BackendCapability::MovesTimedOutCommandsToBackground,
            BackendCapability::AgentInitiatedTurns,
        )?;
        self.require(
            BackendCapability::OutOfBandCapacity,
            BackendCapability::CapacityTelemetry,
        )?;
        Ok(())
    }

    fn require(
        &self,
        capability: BackendCapability,
        required: BackendCapability,
    ) -> Result<(), CapabilityDeclarationError> {
        if self.contains(capability) && !self.contains(required) {
            return Err(CapabilityDeclarationError::MissingRequired {
                capability,
                required,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityDeclarationError {
    MissingRequired {
        capability: BackendCapability,
        required: BackendCapability,
    },
    Conflicting {
        first: BackendCapability,
        second: BackendCapability,
    },
    MissingBackgroundInterruptionScope,
}

impl std::fmt::Display for CapabilityDeclarationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRequired {
                capability,
                required,
            } => write!(formatter, "capability {capability:?} requires {required:?}"),
            Self::Conflicting { first, second } => {
                write!(formatter, "capabilities {first:?} and {second:?} conflict")
            }
            Self::MissingBackgroundInterruptionScope => write!(
                formatter,
                "BackgroundTasks with Interrupt requires an explicit background interruption scope"
            ),
        }
    }
}

impl std::error::Error for CapabilityDeclarationError {}

impl<const N: usize> From<[BackendCapability; N]> for BackendCapabilities {
    fn from(capabilities: [BackendCapability; N]) -> Self {
        Self::new(capabilities)
    }
}
