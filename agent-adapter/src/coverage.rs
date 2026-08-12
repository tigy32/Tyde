use std::collections::BTreeSet;

use protocol::ToolRequestType;

use crate::{BackendCapabilities, BackendCapability};

macro_rules! exhaustive_enum {
    (
        $(#[$attribute:meta])*
        pub enum $name:ident {
            $($variant:ident),+ $(,)?
        }
    ) => {
        $(#[$attribute])*
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: [Self; 0usize $(+ exhaustive_enum!(@one $variant))+] = [
                $(Self::$variant),+
            ];
        }
    };
    (@one $variant:ident) => { 1usize };
}

/// The contract class of every normalized tool request.
///
/// This match is deliberately exhaustive. Adding a protocol tool requires an
/// explicit decision about whether it has generic lifecycle semantics or a
/// special interaction contract before the workspace can compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolContractClass {
    Generic,
    BackgroundCapableCommand,
    UserQuestion,
    UserDecision,
    ChildAgentSpawn,
    ChildAgentMessage,
    ChildAgentAwait,
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum GenericToolContract {
        ModifyFile,
        ReadFiles,
        SearchTypes,
        GetTypeDocs,
        GenerateImage,
        WebSearch,
        ViewImage,
        Sleep,
        Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericToolLifecycleApplicability {
    Required,
    ExplicitlyNotApplicable(&'static str),
}

impl GenericToolContract {
    pub fn for_request(request: &ToolRequestType) -> Option<Self> {
        Some(match request {
            ToolRequestType::ModifyFile { .. } => Self::ModifyFile,
            ToolRequestType::ReadFiles { .. } => Self::ReadFiles,
            ToolRequestType::SearchTypes { .. } => Self::SearchTypes,
            ToolRequestType::GetTypeDocs { .. } => Self::GetTypeDocs,
            ToolRequestType::GenerateImage { .. } => Self::GenerateImage,
            ToolRequestType::WebSearch { .. } => Self::WebSearch,
            ToolRequestType::ViewImage { .. } => Self::ViewImage,
            ToolRequestType::Sleep { .. } => Self::Sleep,
            ToolRequestType::Other { .. } => Self::Other,
            _ => return None,
        })
    }

    pub fn required_capability(self) -> BackendCapability {
        match self {
            Self::ModifyFile => BackendCapability::GenericModifyFile,
            Self::ReadFiles => BackendCapability::GenericReadFiles,
            Self::SearchTypes => BackendCapability::GenericSearchTypes,
            Self::GetTypeDocs => BackendCapability::GenericGetTypeDocs,
            Self::GenerateImage => BackendCapability::GenericGenerateImage,
            Self::WebSearch => BackendCapability::GenericWebSearch,
            Self::ViewImage => BackendCapability::GenericViewImage,
            Self::Sleep => BackendCapability::GenericSleep,
            Self::Other => BackendCapability::GenericOtherTool,
        }
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum GenericToolLifecyclePhase {
        TerminalSuccess,
        TerminalFailure,
        Pending,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GenericToolLifecycleBoundary {
        Natural,
        Interrupt,
        Close,
        CloseResume,
        DisconnectReconnect,
        HostRestartResume,
        Fork,
        TransportClosed,
        TransportClosedResume,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum GenericToolBoundaryTiming {
        BeforeRequest,
        Pending,
        AfterTerminal,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum GenericToolMultiplicity {
        Single,
        HomogeneousTwo,
        HomogeneousThree,
        HeterogeneousTwo,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum GenericToolCallRelation {
        IndependentConcurrent,
        ContingentSequential,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum GenericToolClientTopology {
        Unchanged,
        SecondClientJoin,
        HandoffWithoutZero,
        ZeroClientGap,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenericToolLifecycleCell {
    pub contract: GenericToolContract,
    pub phase: GenericToolLifecyclePhase,
    pub boundary: GenericToolLifecycleBoundary,
    pub activity: ActivityCondition,
    pub timing: GenericToolBoundaryTiming,
    pub multiplicity: GenericToolMultiplicity,
    pub relation: GenericToolCallRelation,
    pub topology: GenericToolClientTopology,
}

impl GenericToolLifecycleCell {
    pub fn classified_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        let mut cells = Vec::new();
        for contract in GenericToolContract::ALL
            .into_iter()
            .filter(|contract| capabilities.contains(contract.required_capability()))
        {
            for phase in GenericToolLifecyclePhase::ALL {
                for boundary in GenericToolLifecycleBoundary::ALL {
                    for activity in ActivityCondition::ALL {
                        cells.push(Self {
                            contract,
                            phase,
                            boundary,
                            activity,
                            timing: if phase == GenericToolLifecyclePhase::Pending {
                                GenericToolBoundaryTiming::Pending
                            } else {
                                GenericToolBoundaryTiming::AfterTerminal
                            },
                            multiplicity: GenericToolMultiplicity::Single,
                            relation: GenericToolCallRelation::IndependentConcurrent,
                            topology: if boundary
                                == GenericToolLifecycleBoundary::DisconnectReconnect
                            {
                                GenericToolClientTopology::ZeroClientGap
                            } else {
                                GenericToolClientTopology::Unchanged
                            },
                        });
                    }
                }
            }
        }
        if capabilities.contains(GenericToolContract::Other.required_capability()) {
            for (multiplicity, relation) in [
                (
                    GenericToolMultiplicity::HomogeneousTwo,
                    GenericToolCallRelation::IndependentConcurrent,
                ),
                (
                    GenericToolMultiplicity::HomogeneousTwo,
                    GenericToolCallRelation::ContingentSequential,
                ),
                (
                    GenericToolMultiplicity::HomogeneousThree,
                    GenericToolCallRelation::IndependentConcurrent,
                ),
                (
                    GenericToolMultiplicity::HomogeneousThree,
                    GenericToolCallRelation::ContingentSequential,
                ),
                (
                    GenericToolMultiplicity::HeterogeneousTwo,
                    GenericToolCallRelation::IndependentConcurrent,
                ),
            ] {
                cells.push(Self {
                    contract: GenericToolContract::Other,
                    phase: GenericToolLifecyclePhase::TerminalSuccess,
                    boundary: GenericToolLifecycleBoundary::Natural,
                    activity: ActivityCondition::ForegroundOnly,
                    timing: GenericToolBoundaryTiming::AfterTerminal,
                    multiplicity,
                    relation,
                    topology: GenericToolClientTopology::Unchanged,
                });
            }
            cells.extend([
                Self {
                    contract: GenericToolContract::Other,
                    phase: GenericToolLifecyclePhase::TerminalSuccess,
                    boundary: GenericToolLifecycleBoundary::Natural,
                    activity: ActivityCondition::ForegroundOnly,
                    timing: GenericToolBoundaryTiming::AfterTerminal,
                    multiplicity: GenericToolMultiplicity::Single,
                    relation: GenericToolCallRelation::IndependentConcurrent,
                    topology: GenericToolClientTopology::SecondClientJoin,
                },
                Self {
                    contract: GenericToolContract::Other,
                    phase: GenericToolLifecyclePhase::TerminalSuccess,
                    boundary: GenericToolLifecycleBoundary::DisconnectReconnect,
                    activity: ActivityCondition::ForegroundOnly,
                    timing: GenericToolBoundaryTiming::AfterTerminal,
                    multiplicity: GenericToolMultiplicity::Single,
                    relation: GenericToolCallRelation::IndependentConcurrent,
                    topology: GenericToolClientTopology::HandoffWithoutZero,
                },
            ]);
            for boundary in [
                GenericToolLifecycleBoundary::Natural,
                GenericToolLifecycleBoundary::Interrupt,
                GenericToolLifecycleBoundary::DisconnectReconnect,
                GenericToolLifecycleBoundary::HostRestartResume,
                GenericToolLifecycleBoundary::Fork,
                GenericToolLifecycleBoundary::CloseResume,
                GenericToolLifecycleBoundary::TransportClosedResume,
            ] {
                cells.push(Self {
                    contract: GenericToolContract::Other,
                    phase: GenericToolLifecyclePhase::TerminalSuccess,
                    boundary,
                    activity: ActivityCondition::ForegroundOnly,
                    timing: GenericToolBoundaryTiming::BeforeRequest,
                    multiplicity: GenericToolMultiplicity::Single,
                    relation: GenericToolCallRelation::IndependentConcurrent,
                    topology: if boundary == GenericToolLifecycleBoundary::DisconnectReconnect {
                        GenericToolClientTopology::ZeroClientGap
                    } else {
                        GenericToolClientTopology::Unchanged
                    },
                });
            }
        }
        cells
    }

    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::classified_for(capabilities)
            .into_iter()
            .filter(|cell| {
                matches!(
                    cell.applicability(capabilities),
                    GenericToolLifecycleApplicability::Required
                )
            })
            .collect()
    }

    pub fn applicability(
        self,
        capabilities: &BackendCapabilities,
    ) -> GenericToolLifecycleApplicability {
        if !capabilities.contains(self.contract.required_capability()) {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "backend does not declare the generic tool contract",
            );
        }
        if !self.activity.supported_by(capabilities) {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "backend does not declare the background mechanism required by the activity condition",
            );
        }
        if self.multiplicity == GenericToolMultiplicity::Single
            && self.relation == GenericToolCallRelation::ContingentSequential
        {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "a single request has no inter-request dependency",
            );
        }
        if self.multiplicity != GenericToolMultiplicity::Single
            && self.contract != GenericToolContract::Other
        {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "multi-request lifecycle ordering is exercised through real generic MCP tools; native provider tools cannot be deterministically batched",
            );
        }
        if self.multiplicity == GenericToolMultiplicity::HeterogeneousTwo
            && self.relation == GenericToolCallRelation::ContingentSequential
        {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "heterogeneous ordering exercises independent admission; contingent ordering is covered with a homogeneous result dependency",
            );
        }
        if self.multiplicity != GenericToolMultiplicity::Single
            && (self.phase != GenericToolLifecyclePhase::TerminalSuccess
                || self.boundary != GenericToolLifecycleBoundary::Natural
                || self.timing != GenericToolBoundaryTiming::AfterTerminal
                || self.activity != ActivityCondition::ForegroundOnly
                || self.topology != GenericToolClientTopology::Unchanged)
        {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "batch cardinality and dependency are orthogonal terminal-ordering probes",
            );
        }
        if self.multiplicity == GenericToolMultiplicity::Single
            && self.relation != GenericToolCallRelation::IndependentConcurrent
        {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "single-request lifecycle uses the canonical independent relation",
            );
        }
        let canonical_timing = match self.phase {
            GenericToolLifecyclePhase::Pending => GenericToolBoundaryTiming::Pending,
            GenericToolLifecyclePhase::TerminalSuccess
            | GenericToolLifecyclePhase::TerminalFailure => {
                GenericToolBoundaryTiming::AfterTerminal
            }
        };
        let before_request_probe = self.contract == GenericToolContract::Other
            && self.phase == GenericToolLifecyclePhase::TerminalSuccess
            && self.activity == ActivityCondition::ForegroundOnly
            && self.multiplicity == GenericToolMultiplicity::Single
            && self.relation == GenericToolCallRelation::IndependentConcurrent
            && self.timing == GenericToolBoundaryTiming::BeforeRequest;
        if self.multiplicity == GenericToolMultiplicity::Single
            && self.timing != canonical_timing
            && !before_request_probe
        {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "the phase names the authoritative lifecycle instant at which this boundary is applied",
            );
        }
        let topology_is_valid = match self.topology {
            GenericToolClientTopology::Unchanged => {
                self.boundary != GenericToolLifecycleBoundary::DisconnectReconnect
            }
            GenericToolClientTopology::ZeroClientGap => {
                self.boundary == GenericToolLifecycleBoundary::DisconnectReconnect
            }
            GenericToolClientTopology::SecondClientJoin => {
                self.contract == GenericToolContract::Other
                    && self.phase == GenericToolLifecyclePhase::TerminalSuccess
                    && self.boundary == GenericToolLifecycleBoundary::Natural
                    && self.activity == ActivityCondition::ForegroundOnly
                    && self.timing == GenericToolBoundaryTiming::AfterTerminal
                    && self.multiplicity == GenericToolMultiplicity::Single
            }
            GenericToolClientTopology::HandoffWithoutZero => {
                self.contract == GenericToolContract::Other
                    && self.phase == GenericToolLifecyclePhase::TerminalSuccess
                    && self.boundary == GenericToolLifecycleBoundary::DisconnectReconnect
                    && self.activity == ActivityCondition::ForegroundOnly
                    && self.timing == GenericToolBoundaryTiming::AfterTerminal
                    && self.multiplicity == GenericToolMultiplicity::Single
            }
        };
        if !topology_is_valid {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                "client topology is only meaningful at its corresponding connection boundary",
            );
        }
        match self.phase {
            GenericToolLifecyclePhase::TerminalSuccess => {}
            GenericToolLifecyclePhase::TerminalFailure
                if !matches!(
                    self.contract,
                    GenericToolContract::ModifyFile
                        | GenericToolContract::ReadFiles
                        | GenericToolContract::ViewImage
                        | GenericToolContract::Other
                ) =>
            {
                return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                    "the native tool exposes no invalid input that both reaches the real tool and guarantees a terminal tool failure rather than model refusal or successful provider handling",
                );
            }
            GenericToolLifecyclePhase::Pending
                if !matches!(
                    self.contract,
                    GenericToolContract::ModifyFile
                        | GenericToolContract::ReadFiles
                        | GenericToolContract::Sleep
                        | GenericToolContract::Other
                ) =>
            {
                return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(
                    "the native tool exposes no blocking input or release control with which the real request can be held pending across a lifecycle boundary",
                );
            }
            _ => {}
        }
        let missing_boundary_capability = match self.boundary {
            GenericToolLifecycleBoundary::Interrupt => {
                (!capabilities.contains(BackendCapability::Interrupt)).then_some("interrupt")
            }
            GenericToolLifecycleBoundary::HostRestartResume
            | GenericToolLifecycleBoundary::CloseResume
            | GenericToolLifecycleBoundary::TransportClosedResume => {
                (!capabilities.contains(BackendCapability::ResumeSession)).then_some("resume")
            }
            GenericToolLifecycleBoundary::Fork => {
                (!capabilities.contains(BackendCapability::ForkSession)).then_some("fork")
            }
            GenericToolLifecycleBoundary::Natural
            | GenericToolLifecycleBoundary::Close
            | GenericToolLifecycleBoundary::DisconnectReconnect
            | GenericToolLifecycleBoundary::TransportClosed => None,
        };
        if let Some(boundary) = missing_boundary_capability {
            return GenericToolLifecycleApplicability::ExplicitlyNotApplicable(match boundary {
                "interrupt" => "backend does not declare interrupt",
                "resume" => "backend does not declare resume",
                "fork" => "backend does not declare fork",
                _ => unreachable!(),
            });
        }
        GenericToolLifecycleApplicability::Required
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum RetryFailureSource {
        ProxyRejected,
        SocketClosed,
        AfterConnect,
        AfterRequestWrite,
        BeforeFirstResponseByte,
        Midstream,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum RetryBoundary {
        Recovery,
        Interrupt,
        Close,
        QueuedFollowUp,
        DisconnectReconnect,
        Exhaustion,
        HostRestartResume,
        Fork,
        ProviderProcessClosed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryLifecycleApplicability {
    Required,
    ExplicitlyNotApplicable(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RetryLifecycleCell {
    pub source: RetryFailureSource,
    pub boundary: RetryBoundary,
    pub activity: ActivityCondition,
}

impl RetryLifecycleCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::RetryTelemetry) {
            return Vec::new();
        }
        RetryFailureSource::ALL
            .into_iter()
            .flat_map(|source| {
                RetryBoundary::ALL.into_iter().flat_map(move |boundary| {
                    ActivityCondition::ALL
                        .into_iter()
                        .map(move |activity| Self {
                            source,
                            boundary,
                            activity,
                        })
                        .filter(move |cell| {
                            matches!(
                                cell.applicability(capabilities),
                                RetryLifecycleApplicability::Required
                            )
                        })
                })
            })
            .collect()
    }

    pub fn applicability(self, capabilities: &BackendCapabilities) -> RetryLifecycleApplicability {
        if !capabilities.contains(BackendCapability::RetryTelemetry) {
            return RetryLifecycleApplicability::ExplicitlyNotApplicable(
                "backend does not declare retry telemetry",
            );
        }
        if !self.activity.supported_by(capabilities) {
            return RetryLifecycleApplicability::ExplicitlyNotApplicable(
                "backend does not declare the ambient background-work capabilities",
            );
        }
        match self.boundary {
            RetryBoundary::Interrupt if !capabilities.contains(BackendCapability::Interrupt) => {
                RetryLifecycleApplicability::ExplicitlyNotApplicable(
                    "backend does not declare interrupt",
                )
            }
            RetryBoundary::HostRestartResume => {
                RetryLifecycleApplicability::ExplicitlyNotApplicable(
                    "host shutdown terminalizes the in-memory active turn; resume starts a new turn",
                )
            }
            RetryBoundary::Fork => RetryLifecycleApplicability::ExplicitlyNotApplicable(
                "fork snapshots settled history and never inherits an active provider request",
            ),
            RetryBoundary::ProviderProcessClosed => {
                RetryLifecycleApplicability::ExplicitlyNotApplicable(
                    "provider-process death is terminal, not a provider-request retry boundary",
                )
            }
            _ => RetryLifecycleApplicability::Required,
        }
    }
}

impl ToolContractClass {
    pub fn for_request(request: &ToolRequestType) -> Self {
        match request {
            ToolRequestType::ModifyFile { .. }
            | ToolRequestType::ReadFiles { .. }
            | ToolRequestType::SearchTypes { .. }
            | ToolRequestType::GetTypeDocs { .. }
            | ToolRequestType::GenerateImage { .. }
            | ToolRequestType::WebSearch { .. }
            | ToolRequestType::ViewImage { .. }
            | ToolRequestType::Sleep { .. }
            | ToolRequestType::Other { .. } => Self::Generic,
            ToolRequestType::RunCommand { .. } => Self::BackgroundCapableCommand,
            ToolRequestType::AskUserQuestion { .. } => Self::UserQuestion,
            ToolRequestType::ExitPlanMode { .. } => Self::UserDecision,
            ToolRequestType::AgentSpawn { .. } => Self::ChildAgentSpawn,
            ToolRequestType::TydeSendAgentMessage { .. } => Self::ChildAgentMessage,
            ToolRequestType::TydeAwaitAgents { .. } => Self::ChildAgentAwait,
        }
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum SpecialToolContract {
        ForegroundCommand,
        BackgroundCommand,
        AskUserQuestion,
        ExitPlanMode,
        AgentSpawn,
        SendAgentMessage,
        AwaitAgents,
        BackgroundSubagent,
    }
}

impl SessionListLifecycleCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| match cell {
                Self::ResumeKeepsIdentityOnce => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                Self::ForkAddsDistinctIdentity => {
                    capabilities.contains(BackendCapability::ForkSession)
                }
                _ => true,
            })
            .collect()
    }
}

impl SpecialToolContract {
    pub fn class(self) -> ToolContractClass {
        match self {
            Self::ForegroundCommand => ToolContractClass::BackgroundCapableCommand,
            Self::BackgroundCommand => ToolContractClass::BackgroundCapableCommand,
            Self::AskUserQuestion => ToolContractClass::UserQuestion,
            Self::ExitPlanMode => ToolContractClass::UserDecision,
            Self::AgentSpawn => ToolContractClass::ChildAgentSpawn,
            Self::SendAgentMessage => ToolContractClass::ChildAgentMessage,
            Self::AwaitAgents => ToolContractClass::ChildAgentAwait,
            Self::BackgroundSubagent => ToolContractClass::ChildAgentSpawn,
        }
    }

    pub fn required_capabilities(self) -> &'static [BackendCapability] {
        match self {
            Self::ForegroundCommand => &[],
            Self::BackgroundCommand => &[BackendCapability::BackgroundTasks],
            Self::AskUserQuestion => &[BackendCapability::UserQuestionRequests],
            Self::ExitPlanMode => &[BackendCapability::PlanApprovalRequests],
            Self::AgentSpawn => &[BackendCapability::ForegroundSubagents],
            Self::BackgroundSubagent => &[BackendCapability::BackgroundSubagents],
            Self::SendAgentMessage | Self::AwaitAgents => &[BackendCapability::AgentControlTools],
        }
    }

    pub fn supported_by(self, capabilities: &BackendCapabilities) -> bool {
        self.required_capabilities()
            .iter()
            .all(|capability| capabilities.contains(*capability))
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ActivityCondition {
        ForegroundOnly,
        OneBackgroundTaskRunning,
        MultipleBackgroundTasksRunning,
        BackgroundCompletesBeforeStimulus,
        BackgroundCompletesDuringStimulus,
        BackgroundCompletesAfterStimulus,
        BackgroundFailsBeforeStimulus,
        BackgroundFailsDuringStimulus,
        BackgroundFailsAfterStimulus,
        OneBackgroundSubagentRunning,
        MultipleBackgroundSubagentsRunning,
        BackgroundSubagentCompletesBeforeStimulus,
        BackgroundSubagentCompletesDuringStimulus,
        BackgroundSubagentCompletesAfterStimulus,
        BackgroundSubagentFailsBeforeStimulus,
        BackgroundSubagentFailsDuringStimulus,
        BackgroundSubagentFailsAfterStimulus,
        MixedBackgroundWorkRunning,
        MixedBackgroundWorkCompletesBeforeStimulus,
        MixedBackgroundWorkCompletesDuringStimulus,
        MixedBackgroundWorkCompletesAfterStimulus,
        MixedBackgroundWorkFailsBeforeStimulus,
        MixedBackgroundWorkFailsDuringStimulus,
        MixedBackgroundWorkFailsAfterStimulus,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AmbientWorkKind {
    None,
    Commands,
    Subagents,
    Mixed,
}

impl ActivityCondition {
    pub fn ambient_work_kind(self) -> AmbientWorkKind {
        match self {
            Self::ForegroundOnly => AmbientWorkKind::None,
            Self::OneBackgroundTaskRunning
            | Self::MultipleBackgroundTasksRunning
            | Self::BackgroundCompletesBeforeStimulus
            | Self::BackgroundCompletesDuringStimulus
            | Self::BackgroundCompletesAfterStimulus
            | Self::BackgroundFailsBeforeStimulus
            | Self::BackgroundFailsDuringStimulus
            | Self::BackgroundFailsAfterStimulus => AmbientWorkKind::Commands,
            Self::OneBackgroundSubagentRunning
            | Self::MultipleBackgroundSubagentsRunning
            | Self::BackgroundSubagentCompletesBeforeStimulus
            | Self::BackgroundSubagentCompletesDuringStimulus
            | Self::BackgroundSubagentCompletesAfterStimulus
            | Self::BackgroundSubagentFailsBeforeStimulus
            | Self::BackgroundSubagentFailsDuringStimulus
            | Self::BackgroundSubagentFailsAfterStimulus => AmbientWorkKind::Subagents,
            Self::MixedBackgroundWorkRunning
            | Self::MixedBackgroundWorkCompletesBeforeStimulus
            | Self::MixedBackgroundWorkCompletesDuringStimulus
            | Self::MixedBackgroundWorkCompletesAfterStimulus
            | Self::MixedBackgroundWorkFailsBeforeStimulus
            | Self::MixedBackgroundWorkFailsDuringStimulus
            | Self::MixedBackgroundWorkFailsAfterStimulus => AmbientWorkKind::Mixed,
        }
    }

    pub fn supported_by(self, capabilities: &BackendCapabilities) -> bool {
        match self.ambient_work_kind() {
            AmbientWorkKind::None => true,
            AmbientWorkKind::Commands => capabilities.contains(BackendCapability::BackgroundTasks),
            AmbientWorkKind::Subagents => {
                capabilities.contains(BackendCapability::BackgroundSubagents)
            }
            AmbientWorkKind::Mixed => {
                capabilities.contains(BackendCapability::BackgroundTasks)
                    && capabilities.contains(BackendCapability::BackgroundSubagents)
            }
        }
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ContractState {
        Requested,
        WaitingForUser,
        Running,
        Terminal,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ContractStimulus {
        ProviderAccepts,
        UserResponds,
        UserRespondsAgain,
        ToolCompletes,
        ToolFails,
        ChildCompletes,
        ChildFails,
        Interrupt,
        Close,
        DisconnectReconnect,
        HostRestartResume,
        Fork,
        TransportClosed,
        TimeoutExpires,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ContractOutcome {
    WaitingForUser,
    Running,
    Completed,
    FailedVisibly,
    StoppedByBoundary,
    Cancelled,
    RemainsWaiting,
    RemainsRunning,
    NewInputOrVisibleRejection,
    RejectedVisiblyNotConsumed,
    PendingInteractionExpiredVisibly,
    PendingInteractionNotCopied,
    Closed,
    RejectedVisiblyAndRemainsWaiting,
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum AgentControlTargetRelation {
        DirectChild,
        Parent,
        Sibling,
        Grandchild,
        UnrelatedRoot,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum AgentControlAuthorizationSet {
        SingleAuthorized,
        SingleUnauthorized,
        MixedAuthorizedFirst,
        MixedUnauthorizedFirst,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum AgentControlRequestMultiplicity {
        Single,
        RepeatedWhileTargetActive,
        RepeatedAfterTargetIdle,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum AgentControlBoundaryRace {
        None,
        ChildSuccessVsInterrupt,
        ChildFailureVsInterrupt,
        ChildSuccessVsClose,
        ChildFailureVsClose,
        ChildSuccessVsHostExpiration,
        ChildFailureVsHostExpiration,
        ChildSuccessVsTransportClose,
        ChildFailureVsTransportClose,
        PendingDurationVsClose,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentControlCoverageCell {
    pub tool: SpecialToolContract,
    pub relation: AgentControlTargetRelation,
    pub authorization: AgentControlAuthorizationSet,
    pub multiplicity: AgentControlRequestMultiplicity,
    pub boundary_race: AgentControlBoundaryRace,
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum NormalizedTurnShape {
        ReasoningBeforeText,
        ToolOnly,
        MultipleAssistantItems,
        EmptyFinal,
        PartialThenError,
        CloseBeforeOutput,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum PlainTerminationMechanism {
        StreamClose,
        TransportDeath,
        HostShutdown,
        ClientDisconnect,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum PlainTerminationPhase {
        BeforeOutput,
        MidOutput,
        AfterOutput,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NormalizedTurnCoverageCell {
    Shape(NormalizedTurnShape),
    Termination {
        mechanism: PlainTerminationMechanism,
        phase: PlainTerminationPhase,
    },
}

exhaustive_enum! {
    /// The lossless public representation of a provider tool call.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum AssistantToolRepresentationCell {
        ExactRequestId,
        ExactRequestName,
        ExactArguments,
        DeclaredOrder,
        UnicodeScalarContentOffset,
        ConnectionReplay,
        ResumeReplay,
        MultiToolDeclaredOrderResumeReplay,
    }
}

impl AssistantToolRepresentationCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| {
                !matches!(
                    *cell,
                    Self::ResumeReplay | Self::MultiToolDeclaredOrderResumeReplay
                ) || capabilities.contains(BackendCapability::ResumeSession)
            })
            .collect()
    }
}

exhaustive_enum! {
    /// Lossless reasoning-stream reconstruction and lifecycle behavior.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ReasoningCoverageCell {
        DeltaReconstruction,
        StableMessageIdentity,
        FinalReasoningMetadata,
        ConnectionReplay,
        ResumeReplay,
    }
}

impl ReasoningCoverageCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::ReasoningDeltas) {
            return Vec::new();
        }
        Self::ALL
            .into_iter()
            .filter(|cell| {
                *cell != Self::ResumeReplay
                    || capabilities.contains(BackendCapability::ResumeSession)
            })
            .collect()
    }
}

exhaustive_enum! {
    /// Ordering, patch semantics, isolation, and replay of late assistant
    /// metadata.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum MessageMetadataCoverageCell {
        TargetsCompletedAssistant,
        OrderedAfterMessage,
        PartialFieldsPreserveExisting,
        ConnectionReplay,
        ForeignMessageIsolation,
        LateUpdateMutatesTargetOnly,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageMetadataCoverageApplicability {
    Required,
    ExplicitlyNotApplicable(&'static str),
}

exhaustive_enum! {
    /// The observable metadata-delivery mechanism selected by a real backend.
    /// This makes the absence of late metadata updates an audited outcome
    /// rather than an unrecorded early return.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum MessageMetadataApplicabilityCell {
        UpdateLifecycleObserved,
        NoUpdateLifecycleExplicitlyNotApplicable,
    }
}

impl MessageMetadataApplicabilityCell {
    pub fn required_for_observed_updates(observed_updates: usize) -> Vec<Self> {
        vec![if observed_updates == 0 {
            Self::NoUpdateLifecycleExplicitlyNotApplicable
        } else {
            Self::UpdateLifecycleObserved
        }]
    }
}

impl MessageMetadataCoverageCell {
    pub fn applicability_for_observed_updates(
        observed_updates: usize,
    ) -> MessageMetadataCoverageApplicability {
        if observed_updates == 0 {
            MessageMetadataCoverageApplicability::ExplicitlyNotApplicable(
                "backend supplied final metadata atomically on StreamEnd and emitted no metadata-update lifecycle",
            )
        } else {
            MessageMetadataCoverageApplicability::Required
        }
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum SessionListLifecycleCell {
        ActiveAppearsOnce,
        ConcurrentClientsAgree,
        ClientReconnectKeepsIdentityOnce,
        ClosedRemainsListedOnce,
        HostRestartKeepsIdentityOnce,
        ResumeKeepsIdentityOnce,
        ForkAddsDistinctIdentity,
        DeleteRemovesIdentity,
        DeleteBroadcastsToAllClients,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LiveCustomizationCell {
        InitialValue,
        ActiveSessionKeepsSpawnSnapshot,
        ClientReconnectKeepsSpawnSnapshot,
        NewSessionUsesLatestValue,
        ResumeUsesLatestValue,
        ForkUsesLatestValue,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum SkillLifecycleCell {
        AllInstalledInvoked,
        ExplicitSelectionInvoked,
        ExplicitSelectionExcludesUnselected,
        ActiveSessionKeepsSpawnSnapshot,
        ClientReconnectKeepsSpawnSnapshot,
        RefreshAffectsNewSession,
        DeletedSkillRemainsInActiveSnapshot,
        ResumeUsesLatestSelection,
        ForkUsesLatestSelection,
        NormalizedNameCollisionFirstInvoked,
        NormalizedNameCollisionSecondInvoked,
        DuplicateIdFirstDirectoryInvoked,
        DuplicateIdLaterDirectoryExcluded,
        DuplicateSelectionIdRejected,
        InvalidMetadataExcluded,
        DeletionRefreshExcludesDeleted,
        MissingSelectedSkillRejectsSpawn,
    }
}

impl SkillLifecycleCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| match cell {
                Self::ResumeUsesLatestSelection => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                Self::ForkUsesLatestSelection => {
                    capabilities.contains(BackendCapability::ForkSession)
                }
                _ => true,
            })
            .collect()
    }
}

impl LiveCustomizationCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| match cell {
                Self::ResumeUsesLatestValue => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                Self::ForkUsesLatestValue => capabilities.contains(BackendCapability::ForkSession),
                _ => true,
            })
            .collect()
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UsageLifecycleCell {
        Completed,
        TransportFailure,
        Cancelled,
        HostRestartFailure,
        ResumedTurn,
        ForkedTurn,
    }
}

exhaustive_enum! {
    /// Model-request usage shapes whose request count and terminal timing differ.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum RequestUsageLifecycleCell {
        Plain,
        ToolLoop,
        InteractionResume,
        Retry,
        MultiTool,
    }
}

impl RequestUsageLifecycleCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::ModelRequestUsageReported) {
            return Vec::new();
        }
        Self::ALL
            .into_iter()
            .filter(|cell| match cell {
                Self::InteractionResume => {
                    capabilities.contains(BackendCapability::UserQuestionRequests)
                }
                Self::Retry => capabilities.contains(BackendCapability::RetryTelemetry),
                _ => true,
            })
            .collect()
    }
}

impl UsageLifecycleCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| match cell {
                Self::Cancelled => capabilities.contains(BackendCapability::Interrupt),
                Self::ResumedTurn => capabilities.contains(BackendCapability::ResumeSession),
                Self::ForkedTurn => capabilities.contains(BackendCapability::ForkSession),
                _ => true,
            })
            .collect()
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ProgressFamily {
        SubAgent,
        Workflow,
        AgentControl,
        BackgroundTask,
        Other,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ProgressTransition {
        RunningToSuccess,
        RunningToFailure,
        RunningAcrossReconnect,
        RunningThenInterrupt,
        RunningThenClose,
        RunningThenHostRestart,
        RunningThenTransportClose,
        RunningForkIsolation,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ProgressSubagentMode {
        NotApplicable,
        Foreground,
        Background,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum CapacityLifecycleCell {
        InitialSnapshot,
        PayloadCardinality,
        ConcurrentClientReplay,
        ProviderRefreshBroadcast,
        RepeatedRefreshConverges,
        ProviderIsolation,
        ZeroClientReconnectReplay,
        HostRestartResets,
        HostRestartRediscovery,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum BackendSetupDiscoveryCell {
        InitialProbe,
        StatusContract,
        NotInstalledStatus,
        UnavailableStatus,
        UnsupportedStatus,
        DiagnosticContract,
        ActionContract,
        ConcurrentClientProbe,
        ReconnectProbe,
        HostRestartProbe,
    }
}

impl BackendSetupDiscoveryCell {
    pub fn applicable_on_current_host(self) -> Result<(), &'static str> {
        match self {
            Self::UnsupportedStatus => Err(
                "all supported release hosts currently provide an install action for every supported backend; Unsupported is reachable only on an unsupported host platform",
            ),
            Self::NotInstalledStatus | Self::UnavailableStatus => Err(
                "real conformance requires the selected provider to remain installed and ready; process-global backend discovery cannot hide or replace that provider safely inside the shared in-process test runner",
            ),
            _ => Ok(()),
        }
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum DynamicSessionDiscoveryCell {
        InitialProbe,
        PendingToReady,
        StatePayloadContract,
        ConcurrentClientProbe,
        ReconnectProbe,
        HostRestartProbe,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum BackendNativeConfigDiscoveryCell {
        InitialProbe,
        ConcurrentClientProbe,
        ExplicitRefresh,
        ReconnectProbe,
        HostRestartProbe,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum McpHttpTransportCell {
        Discovery,
        AuthHeaders,
        BearerToken,
        CorrelatedCallResult,
        CallErrorBounded,
        CallSocketDropBounded,
        MalformedResponseBounded,
        TimeoutBounded,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum McpConnectionOwnershipCell {
        StdioSingleProcess,
        HttpSingleSession,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum McpConfigurationRaceCell {
        StdioEnvironment,
        ConcurrentCalls,
        ToolNameCollisionAcrossServers,
        AddedServerExcludedFromExistingAgent,
        AddedServerAvailableToNewAgent,
        ReplaceDuringCall,
        ReplacementExcludedFromExistingAgent,
        ReplacementAvailableToNewAgent,
        DeleteDuringCall,
        DeletedServerRemainsInExistingAgent,
        DeletedServerUnavailableToNewAgent,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum McpFailureConsistencyCell {
        StartupProcessExit,
        StartupMalformedInitialize,
        StartupMalformedSchema,
        StartupDuplicateSchema,
        StartupInitializeTimeout,
        StartupListTimeout,
        StartupHttpUnreachable,
        RuntimeNamedToolError,
        RuntimeProcessDeath,
        RuntimeProcessDeathRecovery,
        RuntimeWrongResponseId,
        RuntimeMalformedJson,
        RuntimeSocketDrop,
        RuntimeStderrFlood,
        RuntimeDuplicateLateResponse,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpActivityCoverageCell<T> {
    pub contract: T,
    pub activity: ActivityCondition,
}

impl<T: Copy + Ord> McpActivityCoverageCell<T> {
    pub fn required_for(contracts: &[T], capabilities: &BackendCapabilities) -> BTreeSet<Self> {
        contracts
            .iter()
            .copied()
            .flat_map(|contract| {
                ActivityCondition::ALL
                    .into_iter()
                    .filter(|activity| activity.supported_by(capabilities))
                    .map(move |activity| Self { contract, activity })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProgressCoverageCell {
    pub family: ProgressFamily,
    pub subagent_mode: ProgressSubagentMode,
    pub transition: ProgressTransition,
}

impl ProgressCoverageCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> BTreeSet<Self> {
        let families = [
            (
                ProgressFamily::SubAgent,
                capabilities.contains(BackendCapability::ForegroundSubagents)
                    || capabilities.contains(BackendCapability::BackgroundSubagents),
            ),
            (
                ProgressFamily::Workflow,
                capabilities.contains(BackendCapability::WorkflowProgress),
            ),
            (
                ProgressFamily::AgentControl,
                capabilities.contains(BackendCapability::AgentControlTools),
            ),
            (
                ProgressFamily::BackgroundTask,
                capabilities.contains(BackendCapability::BackgroundTasks),
            ),
            (
                ProgressFamily::Other,
                capabilities.contains(BackendCapability::OpaqueToolProgress),
            ),
        ];
        let transitions = ProgressTransition::ALL
            .into_iter()
            .filter(|transition| match transition {
                ProgressTransition::RunningThenInterrupt => {
                    capabilities.contains(BackendCapability::Interrupt)
                }
                ProgressTransition::RunningThenHostRestart => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                ProgressTransition::RunningForkIsolation => {
                    capabilities.contains(BackendCapability::ForkSession)
                }
                _ => true,
            })
            .collect::<Vec<_>>();
        families
            .into_iter()
            .filter(|(_, supported)| *supported)
            .flat_map(|(family, _)| {
                let modes = if family == ProgressFamily::SubAgent {
                    [
                        capabilities
                            .contains(BackendCapability::ForegroundSubagents)
                            .then_some(ProgressSubagentMode::Foreground),
                        capabilities
                            .contains(BackendCapability::BackgroundSubagents)
                            .then_some(ProgressSubagentMode::Background),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                } else {
                    vec![ProgressSubagentMode::NotApplicable]
                };
                let transitions = transitions.clone();
                modes.into_iter().flat_map(move |subagent_mode| {
                    let transitions = transitions.clone();
                    transitions.into_iter().map(move |transition| Self {
                        family,
                        subagent_mode,
                        transition,
                    })
                })
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct EnumeratedCoverageLedger<T: Ord> {
    required: BTreeSet<T>,
    executed: BTreeSet<T>,
    label: &'static str,
}

exhaustive_enum! {
    /// Observable lifecycle boundaries for provider-owned task snapshots.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum TaskUpdateLifecycleCell {
        MultipleSnapshots,
        FailedTransition,
        TitlePreserved,
        SnapshotReplacement,
        SnapshotClear,
        ExactIdentityExactlyOnce,
        ClientZeroGapReconnect,
        HostRestartResumePersistence,
        ForkIsolation,
        InterruptBoundary,
        CloseBoundary,
        TransportBoundary,
    }
}

exhaustive_enum! {
    /// Every currently normalized orchestration payload discriminant.
    ///
    /// This list is intentionally independent of provider implementation. A
    /// backend that advertises orchestration events must expose each payload
    /// its native workflow can reach, and must account explicitly for the
    /// remaining protocol cells in the live matrix.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum OrchestrationPayloadCell {
        AgentStarted,
        AgentCompleted,
        PhaseChanged,
        FanOutStarted,
        WorkerStarted,
        WorkerCompleted,
        FanOutCompleted,
        ConsensusRoundResolved,
        PlanSelected,
        ReviewRoundResolved,
    }
}

exhaustive_enum! {
    /// All workflow phases carried by `OrchestrationPayload::PhaseChanged`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum OrchestrationPhaseCell {
        Reviewing,
        Fixing,
        BuilderPlanning,
        BuilderImplementing,
        BuilderReviewing,
        BuilderFixing,
        SwarmPlanning,
        SwarmPlanFanOut,
        SwarmConsensus,
        SwarmImplementing,
        SwarmFanOut,
        SwarmIntegration,
        SwarmFixing,
    }
}

exhaustive_enum! {
    /// Outcome cells that are valid at each orchestration terminal site.
    ///
    /// Open fan-outs intentionally have no synthetic `Aborted` worker or
    /// fan-out terminal: cancellation closes them at the operation boundary.
    /// Agent stack disposal, by contrast, has an explicit aborted terminal.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum OrchestrationOutcomeCell {
        AgentSucceeded,
        AgentFailed,
        AgentAborted,
        WorkerSucceeded,
        WorkerFailed,
        FanOutSucceeded,
        FanOutFailed,
        OpenFanOutCancelledWithoutSyntheticTerminals,
    }
}

exhaustive_enum! {
    /// Variant-bearing metadata whose branches change observable semantics.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum OrchestrationMetadataCell {
        RootOrigin,
        WorkflowOrigin,
        ToolOriginCorrelatesToolCall,
        ParentIdentityCorrelates,
        UnpinnedModel,
        PinnedModel,
        InteractiveAgent,
        ReviewedWorker,
        UnreviewedWorker,
        ConsensusEliminatesCandidate,
        ConsensusKeepsAllCandidates,
        PlanSelectsCandidate,
        PlanSelectsSinglePlannerWithoutCandidate,
        PanelEndorsed,
        PanelRevised,
        PanelNoPosition,
        PanelFailed,
        ReviewApproved,
        ReviewRejected,
        ReviewRoundLimitReached,
    }
}

exhaustive_enum! {
    /// Ordering and identity relationships that make orchestration events a
    /// coherent tree rather than an uncorrelated bag of notifications.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum OrchestrationCorrelationCell {
        AgentStartPrecedesAgentEvents,
        ParentStartsBeforeChild,
        AgentTerminalExactlyOnce,
        NoAgentEventsAfterTerminal,
        FanOutStartPrecedesWorkerEvents,
        DeclaredWorkersMatchObservedWorkers,
        WorkerStartPrecedesWorkerTerminal,
        WorkerTerminalExactlyOnce,
        FanOutTerminalFollowsAllWorkers,
        FanOutTerminalExactlyOnce,
        ConcurrentWorkerCompletionOrderIndependent,
        ConsensusRoundsStrictlyIncrease,
        ReviewRoundsStrictlyIncrease,
        PlanSelectedExactlyOnce,
        PayloadAgentIdentityStable,
        ReplayPreservesEventIdentityAndOrder,
    }
}

exhaustive_enum! {
    /// Session and transport boundaries that orchestration state crosses.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum OrchestrationLifecycleCell {
        SuccessfulCompletion,
        FailedCompletion,
        InterruptClosesOpenWork,
        CloseClosesOpenWork,
        DisconnectReconnectReplaysExactlyOnce,
        HostRestartResumeReplaysExactlyOnce,
        ForkCopiesHistoryButIsolatesFutureEvents,
        AgentSwitchAbortsOldStackBeforeNewRoot,
        ConversationResetAbortsOldStackBeforeClear,
    }
}

impl OrchestrationLifecycleCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::OrchestrationEvents) {
            return Vec::new();
        }
        Self::ALL
            .into_iter()
            .filter(|cell| match cell {
                Self::InterruptClosesOpenWork => {
                    capabilities.contains(BackendCapability::Interrupt)
                }
                Self::HostRestartResumeReplaysExactlyOnce => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                Self::ForkCopiesHistoryButIsolatesFutureEvents => {
                    capabilities.contains(BackendCapability::ForkSession)
                }
                _ => true,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskUpdateLifecycleApplicability {
    Required,
    ExplicitlyNotApplicable(&'static str),
}

impl TaskUpdateLifecycleCell {
    pub fn applicability(
        self,
        capabilities: &BackendCapabilities,
    ) -> TaskUpdateLifecycleApplicability {
        match self {
            Self::SnapshotReplacement
                if !capabilities.contains(BackendCapability::TaskListReplacement) =>
            {
                TaskUpdateLifecycleApplicability::ExplicitlyNotApplicable(
                    "provider exposes no model-reachable whole-list replacement",
                )
            }
            Self::SnapshotClear if !capabilities.contains(BackendCapability::TaskListClear) => {
                TaskUpdateLifecycleApplicability::ExplicitlyNotApplicable(
                    "provider exposes no model-reachable task-list clear operation",
                )
            }
            _ => TaskUpdateLifecycleApplicability::Required,
        }
    }

    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| {
                matches!(
                    cell.applicability(capabilities),
                    TaskUpdateLifecycleApplicability::Required
                )
            })
            .filter(|cell| match cell {
                Self::HostRestartResumePersistence => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                Self::ForkIsolation => capabilities.contains(BackendCapability::ForkSession),
                Self::InterruptBoundary => capabilities.contains(BackendCapability::Interrupt),
                _ => true,
            })
            .collect()
    }

    pub fn core_required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::required_for(capabilities)
            .into_iter()
            .filter(|cell| !matches!(cell, Self::SnapshotReplacement | Self::SnapshotClear))
            .collect()
    }
}

impl<T: Copy + Ord + std::fmt::Debug> EnumeratedCoverageLedger<T> {
    pub fn new(required: impl IntoIterator<Item = T>, label: &'static str) -> Self {
        Self {
            required: required.into_iter().collect(),
            executed: BTreeSet::new(),
            label,
        }
    }

    pub fn record(&mut self, cell: T) -> Result<(), ConformanceCoverageError> {
        if !self.required.contains(&cell) {
            return Err(ConformanceCoverageError {
                message: format!("recorded non-contract {} cell {cell:?}", self.label),
            });
        }
        if !self.executed.insert(cell) {
            return Err(ConformanceCoverageError {
                message: format!("{} cell executed twice {cell:?}", self.label),
            });
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), ConformanceCoverageError> {
        let missing = self
            .required
            .difference(&self.executed)
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConformanceCoverageError {
                message: format!(
                    "missing {} {} cells: {missing:?}",
                    missing.len(),
                    self.label
                ),
            })
        }
    }
}

#[derive(Debug)]
pub struct NormalizedTurnCoverageLedger {
    required: BTreeSet<NormalizedTurnCoverageCell>,
    executed: BTreeSet<NormalizedTurnCoverageCell>,
}

impl NormalizedTurnCoverageLedger {
    pub fn for_capabilities(capabilities: &BackendCapabilities) -> Self {
        let mut required = BTreeSet::new();
        for shape in NormalizedTurnShape::ALL {
            if shape != NormalizedTurnShape::ReasoningBeforeText
                || capabilities.contains(BackendCapability::ReasoningDeltas)
            {
                required.insert(NormalizedTurnCoverageCell::Shape(shape));
            }
        }
        for mechanism in PlainTerminationMechanism::ALL {
            for phase in PlainTerminationPhase::ALL {
                required.insert(NormalizedTurnCoverageCell::Termination { mechanism, phase });
            }
        }
        Self {
            required,
            executed: BTreeSet::new(),
        }
    }

    pub fn shapes_for_capabilities(capabilities: &BackendCapabilities) -> Self {
        let required = Self::for_capabilities(capabilities)
            .required
            .into_iter()
            .filter(|cell| matches!(cell, NormalizedTurnCoverageCell::Shape(_)))
            .collect();
        Self {
            required,
            executed: BTreeSet::new(),
        }
    }

    pub fn terminations_for_capabilities(capabilities: &BackendCapabilities) -> Self {
        let required = Self::for_capabilities(capabilities)
            .required
            .into_iter()
            .filter(|cell| matches!(cell, NormalizedTurnCoverageCell::Termination { .. }))
            .collect();
        Self {
            required,
            executed: BTreeSet::new(),
        }
    }

    pub fn record(
        &mut self,
        cell: NormalizedTurnCoverageCell,
    ) -> Result<(), ConformanceCoverageError> {
        if !self.required.contains(&cell) {
            return Err(ConformanceCoverageError {
                message: format!("recorded non-contract normalized-turn cell {cell:?}"),
            });
        }
        if !self.executed.insert(cell) {
            return Err(ConformanceCoverageError {
                message: format!("normalized-turn cell executed twice {cell:?}"),
            });
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), ConformanceCoverageError> {
        let missing = self
            .required
            .difference(&self.executed)
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConformanceCoverageError {
                message: format!(
                    "missing {} normalized-turn conformance cells: {missing:?}",
                    missing.len()
                ),
            })
        }
    }
}

impl AgentControlCoverageCell {
    pub fn required() -> BTreeSet<Self> {
        [
            SpecialToolContract::SendAgentMessage,
            SpecialToolContract::AwaitAgents,
        ]
        .into_iter()
        .flat_map(|tool| {
            AgentControlTargetRelation::ALL
                .into_iter()
                .map(move |relation| (tool, relation))
        })
        .flat_map(|(tool, relation)| {
            AgentControlAuthorizationSet::ALL
                .into_iter()
                .map(move |authorization| (tool, relation, authorization))
        })
        .flat_map(|(tool, relation, authorization)| {
            AgentControlRequestMultiplicity::ALL
                .into_iter()
                .map(move |multiplicity| (tool, relation, authorization, multiplicity))
        })
        .flat_map(|(tool, relation, authorization, multiplicity)| {
            AgentControlBoundaryRace::ALL
                .into_iter()
                .map(move |boundary_race| Self {
                    tool,
                    relation,
                    authorization,
                    multiplicity,
                    boundary_race,
                })
        })
        .filter(Self::is_valid_contract_cell)
        .collect()
    }

    pub fn required_for(capabilities: &BackendCapabilities) -> BTreeSet<Self> {
        if !capabilities.contains(BackendCapability::AgentControlTools) {
            return BTreeSet::new();
        }
        Self::required()
            .into_iter()
            .filter(|cell| {
                !matches!(
                    cell.boundary_race,
                    AgentControlBoundaryRace::ChildSuccessVsInterrupt
                        | AgentControlBoundaryRace::ChildFailureVsInterrupt
                ) || capabilities.contains(BackendCapability::Interrupt)
            })
            .collect()
    }

    fn is_valid_contract_cell(cell: &Self) -> bool {
        if cell.boundary_race != AgentControlBoundaryRace::None {
            return cell.relation == AgentControlTargetRelation::DirectChild
                && cell.authorization == AgentControlAuthorizationSet::SingleAuthorized
                && cell.multiplicity == AgentControlRequestMultiplicity::Single;
        }
        let authorization_applies = match cell.authorization {
            AgentControlAuthorizationSet::SingleAuthorized => {
                cell.relation == AgentControlTargetRelation::DirectChild
            }
            AgentControlAuthorizationSet::SingleUnauthorized => {
                cell.relation != AgentControlTargetRelation::DirectChild
            }
            AgentControlAuthorizationSet::MixedAuthorizedFirst
            | AgentControlAuthorizationSet::MixedUnauthorizedFirst => {
                cell.tool == SpecialToolContract::AwaitAgents
                    && cell.relation == AgentControlTargetRelation::Sibling
            }
        };
        if !authorization_applies {
            return false;
        }
        match cell.multiplicity {
            AgentControlRequestMultiplicity::Single => true,
            AgentControlRequestMultiplicity::RepeatedWhileTargetActive => {
                cell.tool == SpecialToolContract::SendAgentMessage
                    && cell.authorization == AgentControlAuthorizationSet::SingleAuthorized
            }
            AgentControlRequestMultiplicity::RepeatedAfterTargetIdle => {
                cell.authorization == AgentControlAuthorizationSet::SingleAuthorized
            }
        }
    }
}

#[derive(Debug)]
pub struct AgentControlCoverageLedger {
    required: BTreeSet<AgentControlCoverageCell>,
    executed: BTreeSet<AgentControlCoverageCell>,
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum InputAdmissionState {
        Idle,
        ModelStreaming,
        ForegroundToolPending,
        UserQuestionWaiting,
        PlanApprovalWaiting,
        McpToolPending,
        GenericOtherPending,
        NativeSpawnPending,
        AgentAwaitPending,
        CompactionPending,
        BackgroundOnlyIdle,
        AgentInitiatedTurn,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum InputAdmissionKind {
        PlainMessage,
        MultipleMessages,
        InteractionResponse,
        AskResponseCurrent,
        AskResponseStale,
        AskResponseWrongKind,
        AskResponseForeignStream,
        PlanResponseCurrent,
        PlanResponseStale,
        PlanResponseWrongKind,
        PlanResponseForeignStream,
        ImageMessage,
        MultipleImageMessages,
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum InputAdmissionAction {
        Admit,
        Enqueue,
        EditQueued,
        CancelQueued,
        SendQueuedNow,
        InterruptThenSend,
        ClientReconnect,
        HostRestart,
        Fork,
        TransportClosed,
        Close,
        BackgroundTerminal,
        BackgroundFailureTerminal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputAdmissionCoverageCell {
    pub activity: ActivityCondition,
    pub state: InputAdmissionState,
    pub input: InputAdmissionKind,
    pub action: InputAdmissionAction,
}

impl InputAdmissionCoverageCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> BTreeSet<Self> {
        let mut required = BTreeSet::new();
        for state in InputAdmissionState::ALL {
            let supported = match state {
                InputAdmissionState::UserQuestionWaiting => {
                    capabilities.contains(BackendCapability::UserQuestionRequests)
                }
                InputAdmissionState::PlanApprovalWaiting => {
                    capabilities.contains(BackendCapability::PlanApprovalRequests)
                }
                InputAdmissionState::BackgroundOnlyIdle => {
                    capabilities.contains(BackendCapability::BackgroundTasks)
                }
                InputAdmissionState::AgentInitiatedTurn => {
                    capabilities.contains(BackendCapability::AgentInitiatedTurns)
                        && capabilities.contains(BackendCapability::BackgroundTasks)
                }
                InputAdmissionState::McpToolPending => {
                    capabilities.contains(BackendCapability::StartupMcpServers)
                }
                InputAdmissionState::GenericOtherPending => {
                    capabilities.contains(BackendCapability::GenericOtherTool)
                }
                InputAdmissionState::NativeSpawnPending => {
                    capabilities.contains(BackendCapability::ForegroundSubagents)
                }
                InputAdmissionState::AgentAwaitPending => {
                    capabilities.contains(BackendCapability::AgentControlTools)
                }
                InputAdmissionState::CompactionPending => {
                    capabilities.contains(BackendCapability::CompactionReported)
                }
                _ => true,
            };
            if !supported {
                continue;
            }
            let activities: Vec<_> = ActivityCondition::ALL
                .into_iter()
                .filter(|activity| activity.supported_by(capabilities))
                .collect();
            for activity in activities {
                for input in [
                    InputAdmissionKind::PlainMessage,
                    InputAdmissionKind::MultipleMessages,
                ] {
                    required.insert(Self {
                        activity,
                        state,
                        input,
                        action: InputAdmissionAction::Admit,
                    });
                }
                if capabilities.contains(BackendCapability::ImageInput) {
                    for input in [
                        InputAdmissionKind::ImageMessage,
                        InputAdmissionKind::MultipleImageMessages,
                    ] {
                        required.insert(Self {
                            activity,
                            state,
                            input,
                            action: InputAdmissionAction::Admit,
                        });
                    }
                }
                if matches!(
                    state,
                    InputAdmissionState::ModelStreaming
                        | InputAdmissionState::ForegroundToolPending
                        | InputAdmissionState::UserQuestionWaiting
                        | InputAdmissionState::PlanApprovalWaiting
                        | InputAdmissionState::McpToolPending
                        | InputAdmissionState::GenericOtherPending
                        | InputAdmissionState::NativeSpawnPending
                        | InputAdmissionState::AgentAwaitPending
                        | InputAdmissionState::CompactionPending
                        | InputAdmissionState::AgentInitiatedTurn
                ) {
                    let mut actions = vec![
                        InputAdmissionAction::Enqueue,
                        InputAdmissionAction::EditQueued,
                        InputAdmissionAction::CancelQueued,
                        InputAdmissionAction::SendQueuedNow,
                        InputAdmissionAction::ClientReconnect,
                        InputAdmissionAction::Close,
                        InputAdmissionAction::TransportClosed,
                    ];
                    if capabilities.contains(BackendCapability::Interrupt) {
                        actions.push(InputAdmissionAction::InterruptThenSend);
                    }
                    if capabilities.contains(BackendCapability::ResumeSession) {
                        actions.push(InputAdmissionAction::HostRestart);
                    }
                    if capabilities.contains(BackendCapability::ForkSession) {
                        actions.push(InputAdmissionAction::Fork);
                    }
                    for action in actions {
                        for input in [
                            InputAdmissionKind::PlainMessage,
                            InputAdmissionKind::MultipleMessages,
                        ] {
                            required.insert(Self {
                                activity,
                                state,
                                input,
                                action,
                            });
                        }
                        if capabilities.contains(BackendCapability::ImageInput) {
                            for input in [
                                InputAdmissionKind::ImageMessage,
                                InputAdmissionKind::MultipleImageMessages,
                            ] {
                                required.insert(Self {
                                    activity,
                                    state,
                                    input,
                                    action,
                                });
                            }
                        }
                    }
                }
                for input in [
                    InputAdmissionKind::AskResponseCurrent,
                    InputAdmissionKind::AskResponseStale,
                    InputAdmissionKind::AskResponseForeignStream,
                ] {
                    if capabilities.contains(BackendCapability::UserQuestionRequests) {
                        required.insert(Self {
                            activity,
                            state,
                            input,
                            action: InputAdmissionAction::Admit,
                        });
                    }
                }
                for input in [
                    InputAdmissionKind::PlanResponseCurrent,
                    InputAdmissionKind::PlanResponseStale,
                    InputAdmissionKind::PlanResponseForeignStream,
                ] {
                    if capabilities.contains(BackendCapability::PlanApprovalRequests) {
                        required.insert(Self {
                            activity,
                            state,
                            input,
                            action: InputAdmissionAction::Admit,
                        });
                    }
                }
                if capabilities.contains(BackendCapability::UserQuestionRequests)
                    && capabilities.contains(BackendCapability::PlanApprovalRequests)
                {
                    for input in [
                        InputAdmissionKind::AskResponseWrongKind,
                        InputAdmissionKind::PlanResponseWrongKind,
                    ] {
                        required.insert(Self {
                            activity,
                            state,
                            input,
                            action: InputAdmissionAction::Admit,
                        });
                    }
                }
                if matches!(
                    state,
                    InputAdmissionState::UserQuestionWaiting
                        | InputAdmissionState::PlanApprovalWaiting
                ) {
                    let mut actions = vec![
                        InputAdmissionAction::Admit,
                        InputAdmissionAction::Enqueue,
                        InputAdmissionAction::EditQueued,
                        InputAdmissionAction::CancelQueued,
                        InputAdmissionAction::SendQueuedNow,
                        InputAdmissionAction::ClientReconnect,
                        InputAdmissionAction::Close,
                        InputAdmissionAction::TransportClosed,
                    ];
                    if capabilities.contains(BackendCapability::Interrupt) {
                        actions.push(InputAdmissionAction::InterruptThenSend);
                    }
                    if capabilities.contains(BackendCapability::ResumeSession) {
                        actions.push(InputAdmissionAction::HostRestart);
                    }
                    if capabilities.contains(BackendCapability::ForkSession) {
                        actions.push(InputAdmissionAction::Fork);
                    }
                    for action in actions {
                        required.insert(Self {
                            activity,
                            state,
                            input: InputAdmissionKind::InteractionResponse,
                            action,
                        });
                    }
                }
            }
            if matches!(
                state,
                InputAdmissionState::UserQuestionWaiting | InputAdmissionState::PlanApprovalWaiting
            ) && capabilities.contains(BackendCapability::BackgroundTasks)
            {
                for (activity, action) in [
                    (
                        ActivityCondition::BackgroundCompletesDuringStimulus,
                        InputAdmissionAction::BackgroundTerminal,
                    ),
                    (
                        ActivityCondition::BackgroundFailsDuringStimulus,
                        InputAdmissionAction::BackgroundFailureTerminal,
                    ),
                ] {
                    required.insert(Self {
                        activity,
                        state,
                        input: InputAdmissionKind::InteractionResponse,
                        action,
                    });
                }
            }
            if matches!(
                state,
                InputAdmissionState::UserQuestionWaiting | InputAdmissionState::PlanApprovalWaiting
            ) && capabilities.contains(BackendCapability::BackgroundSubagents)
            {
                for (activity, action) in [
                    (
                        ActivityCondition::BackgroundSubagentCompletesDuringStimulus,
                        InputAdmissionAction::BackgroundTerminal,
                    ),
                    (
                        ActivityCondition::BackgroundSubagentFailsDuringStimulus,
                        InputAdmissionAction::BackgroundFailureTerminal,
                    ),
                ] {
                    required.insert(Self {
                        activity,
                        state,
                        input: InputAdmissionKind::InteractionResponse,
                        action,
                    });
                }
            }
            if matches!(
                state,
                InputAdmissionState::UserQuestionWaiting | InputAdmissionState::PlanApprovalWaiting
            ) && capabilities.contains(BackendCapability::BackgroundTasks)
                && capabilities.contains(BackendCapability::BackgroundSubagents)
            {
                for (activity, action) in [
                    (
                        ActivityCondition::MixedBackgroundWorkCompletesDuringStimulus,
                        InputAdmissionAction::BackgroundTerminal,
                    ),
                    (
                        ActivityCondition::MixedBackgroundWorkFailsDuringStimulus,
                        InputAdmissionAction::BackgroundFailureTerminal,
                    ),
                ] {
                    required.insert(Self {
                        activity,
                        state,
                        input: InputAdmissionKind::InteractionResponse,
                        action,
                    });
                }
            }
        }
        required
    }
}

#[derive(Debug)]
pub struct InputAdmissionCoverageLedger {
    required: BTreeSet<InputAdmissionCoverageCell>,
    executed: BTreeSet<InputAdmissionCoverageCell>,
}

exhaustive_enum! {
    /// Every observable attachment-shape and validation branch accepted by
    /// the public image-input protocol.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ImageAttachmentCell {
        PngRoundTrip,
        JpegRoundTrip,
        GifRoundTrip,
        WebpRoundTrip,
        MultipleAttachmentOrder,
        ProseWithImages,
        ImageWithoutProse,
        ExplicitEmptyListIsolation,
        FollowUpIsolation,
        InvalidEmptyMediaType,
        InvalidWhitespaceMediaType,
        InvalidNonImageMediaType,
        InvalidUnsupportedMediaType,
        InvalidEmptyData,
        InvalidWhitespaceData,
        InvalidSurroundingWhitespace,
        InvalidMalformedBase64,
        InvalidNonImageBytes,
        InvalidTruncatedBytes,
        InvalidDataUrl,
        InvalidMediaTypeContentMismatch,
        InvalidMixedPayloadAtomicity,
        RejectionCreatesNoSession,
        RejectionDoesNotPersist,
        PostRejectionRecovery,
        ProviderByteLimit,
        ProviderDimensionLimit,
        ProviderAttachmentCountLimit,
        ProviderRejectsOtherwiseValidImage,
        ProviderRejectsHeaderValidCorruptImage,
    }
}

exhaustive_enum! {
    /// Image-bearing input across every host lifecycle boundary that can be
    /// induced without fabricating provider output.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ImageLifecycleCell {
        ConnectionReplay,
        ForkCopiesHistory,
        ForkCurrentImageIsolation,
        CloseBeforeResume,
        HostRestartResumeMemory,
        ResumeReplayBytes,
        QueuedBytesPreserved,
        QueuedMultipleOrderPreserved,
        QueuedEditReplacesBytes,
        QueuedCancelRemovesPayload,
        QueuedSendNowConsumesOnce,
        QueuedInterruptThenDispatches,
        QueuedReconnectPreservesBytes,
        QueuedHostRestartResumePreservesBytes,
        QueuedForkIsolation,
        QueuedCloseDoesNotConsume,
        QueuedTransportCloseDoesNotConsume,
        ConcurrentStreamsIsolated,
        ConcurrentClientsIsolated,
        AskUserResponseWithImages,
        PlanApprovalResponseWithImages,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageAttachmentActivityCell {
    pub attachment: ImageAttachmentCell,
    pub activity: ActivityCondition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageLifecycleActivityCell {
    pub lifecycle: ImageLifecycleCell,
    pub activity: ActivityCondition,
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum InteractionImagePayloadCell {
        Valid,
        Invalid,
        MixedValidInvalid,
        ProviderRejected,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InteractionImageActivityCell {
    pub contract: InteractionConcurrencyContract,
    pub payload: InteractionImagePayloadCell,
    pub activity: ActivityCondition,
}

impl InteractionImageActivityCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::ImageInput) {
            return Vec::new();
        }
        [
            InteractionConcurrencyContract::AskUserQuestion,
            InteractionConcurrencyContract::ExitPlanMode,
        ]
        .into_iter()
        .filter(|contract| match contract {
            InteractionConcurrencyContract::AskUserQuestion => {
                capabilities.contains(BackendCapability::UserQuestionRequests)
            }
            InteractionConcurrencyContract::ExitPlanMode => {
                capabilities.contains(BackendCapability::PlanApprovalRequests)
            }
            InteractionConcurrencyContract::AskAndPlan => false,
        })
        .flat_map(|contract| {
            InteractionImagePayloadCell::ALL
                .into_iter()
                .flat_map(move |payload| {
                    ActivityCondition::ALL
                        .into_iter()
                        .filter(|activity| activity.supported_by(capabilities))
                        .map(move |activity| Self {
                            contract,
                            payload,
                            activity,
                        })
                })
        })
        .collect()
    }
}

impl ImageAttachmentActivityCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::ImageInput) {
            return Vec::new();
        }
        ImageAttachmentCell::required()
            .into_iter()
            .flat_map(|attachment| {
                ActivityCondition::ALL
                    .into_iter()
                    .filter(|activity| activity.supported_by(capabilities))
                    .map(move |activity| Self {
                        attachment,
                        activity,
                    })
            })
            .collect()
    }
}

impl ImageLifecycleActivityCell {
    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        if !capabilities.contains(BackendCapability::ImageInput) {
            return Vec::new();
        }
        ImageLifecycleCell::required_for(capabilities)
            .into_iter()
            .flat_map(|lifecycle| {
                ActivityCondition::ALL
                    .into_iter()
                    .filter(|activity| activity.supported_by(capabilities))
                    .map(move |activity| Self {
                        lifecycle,
                        activity,
                    })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCoverageApplicability {
    Required,
    ExplicitlyNotApplicable(&'static str),
}

impl ImageAttachmentCell {
    pub fn applicability(self) -> ImageCoverageApplicability {
        let _ = self;
        ImageCoverageApplicability::Required
    }

    pub fn required() -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| matches!(cell.applicability(), ImageCoverageApplicability::Required))
            .collect()
    }
}

impl ImageLifecycleCell {
    pub fn applicability(self, capabilities: &BackendCapabilities) -> ImageCoverageApplicability {
        match self {
            Self::ForkCopiesHistory
            | Self::ForkCurrentImageIsolation
            | Self::QueuedForkIsolation
                if !capabilities.contains(BackendCapability::ForkSession) =>
            {
                ImageCoverageApplicability::ExplicitlyNotApplicable(
                    "backend does not advertise session fork",
                )
            }
            Self::CloseBeforeResume
            | Self::HostRestartResumeMemory
            | Self::ResumeReplayBytes
            | Self::QueuedHostRestartResumePreservesBytes
                if !capabilities.contains(BackendCapability::ResumeSession) =>
            {
                ImageCoverageApplicability::ExplicitlyNotApplicable(
                    "backend does not advertise session resume",
                )
            }
            Self::QueuedInterruptThenDispatches
                if !capabilities.contains(BackendCapability::Interrupt) =>
            {
                ImageCoverageApplicability::ExplicitlyNotApplicable(
                    "backend does not advertise interrupt",
                )
            }
            Self::AskUserResponseWithImages
                if !capabilities.contains(BackendCapability::UserQuestionRequests) =>
            {
                ImageCoverageApplicability::ExplicitlyNotApplicable(
                    "backend exposes no native ask-user interaction",
                )
            }
            Self::PlanApprovalResponseWithImages
                if !capabilities.contains(BackendCapability::PlanApprovalRequests) =>
            {
                ImageCoverageApplicability::ExplicitlyNotApplicable(
                    "backend exposes no native plan-approval interaction",
                )
            }
            _ => ImageCoverageApplicability::Required,
        }
    }

    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|cell| {
                matches!(
                    cell.applicability(capabilities),
                    ImageCoverageApplicability::Required
                )
            })
            .collect()
    }
}

impl InputAdmissionCoverageLedger {
    pub fn for_capabilities(capabilities: &BackendCapabilities) -> Self {
        Self {
            required: InputAdmissionCoverageCell::required_for(capabilities),
            executed: BTreeSet::new(),
        }
    }

    pub fn record(
        &mut self,
        cell: InputAdmissionCoverageCell,
    ) -> Result<(), ConformanceCoverageError> {
        if !self.required.contains(&cell) {
            return Err(ConformanceCoverageError {
                message: format!("recorded non-contract input-admission cell {cell:?}"),
            });
        }
        if !self.executed.insert(cell) {
            return Err(ConformanceCoverageError {
                message: format!("input-admission cell executed twice {cell:?}"),
            });
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), ConformanceCoverageError> {
        let missing = self
            .required
            .difference(&self.executed)
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConformanceCoverageError {
                message: format!(
                    "missing {} input-admission conformance cells: {missing:?}",
                    missing.len()
                ),
            })
        }
    }
}

impl Default for AgentControlCoverageLedger {
    fn default() -> Self {
        Self {
            required: AgentControlCoverageCell::required(),
            executed: BTreeSet::new(),
        }
    }
}

impl AgentControlCoverageLedger {
    pub fn for_capabilities(capabilities: &BackendCapabilities) -> Self {
        let required = AgentControlCoverageCell::required_for(capabilities);
        Self {
            required,
            executed: BTreeSet::new(),
        }
    }

    pub fn record(
        &mut self,
        cell: AgentControlCoverageCell,
    ) -> Result<(), ConformanceCoverageError> {
        if !self.required.contains(&cell) {
            return Err(ConformanceCoverageError {
                message: format!("recorded non-contract agent-control cell {cell:?}"),
            });
        }
        if !self.executed.insert(cell) {
            return Err(ConformanceCoverageError {
                message: format!("agent-control cell executed twice {cell:?}"),
            });
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), ConformanceCoverageError> {
        let missing = self
            .required
            .difference(&self.executed)
            .copied()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ConformanceCoverageError {
                message: format!(
                    "missing {} agent-control conformance cells: {missing:?}",
                    missing.len()
                ),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractTransitionApplicability {
    Required(ContractOutcome),
    ExplicitlyNotApplicable(&'static str),
}

exhaustive_enum! {
    /// Every semantically distinct way a typed human rendezvous can interact
    /// with another pending rendezvous or a lifecycle boundary.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum InteractionConcurrencyScenario {
        TwoAgentsForwardResponseOrder,
        TwoAgentsReverseResponseOrder,
        TwoAgentsConcurrentResponses,
        TwoConcurrentClients,
        WrongStreamResponse,
        DuplicateResponseAfterReconnect,
        SequentialStaleToolIds,
        ResponseWithProse,
        ResponseWithImages,
        ResponseWhileInputQueued,
        ResponseRacesInterrupt,
        ResponseRacesClose,
        ResponseRacesTransportClose,
        ResponseRacesReconnect,
        ResponseRacesHostRestart,
        NaturalSameToolIdCrossStream,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InteractionConcurrencyContract {
    AskUserQuestion,
    ExitPlanMode,
    AskAndPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionConcurrencyApplicability {
    Required,
    ExplicitlyNotApplicable(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InteractionConcurrencyCell {
    pub contract: InteractionConcurrencyContract,
    pub scenario: InteractionConcurrencyScenario,
}

impl InteractionConcurrencyCell {
    pub fn applicability(self) -> InteractionConcurrencyApplicability {
        use InteractionConcurrencyContract as Contract;
        use InteractionConcurrencyScenario as Scenario;

        match (self.contract, self.scenario) {
            // Tool-call identifiers are opaque provider output. A conformance
            // harness cannot force two real providers' live streams to choose
            // the same value without fabricating backend behavior.
            (_, Scenario::NaturalSameToolIdCrossStream) => {
                InteractionConcurrencyApplicability::ExplicitlyNotApplicable(
                    "opaque provider-generated tool ids cannot be forced to collide",
                )
            }
            (Contract::AskAndPlan, Scenario::TwoAgentsForwardResponseOrder)
            | (Contract::AskAndPlan, Scenario::TwoAgentsReverseResponseOrder)
            | (Contract::AskAndPlan, Scenario::TwoAgentsConcurrentResponses)
            | (Contract::AskAndPlan, Scenario::TwoConcurrentClients)
            | (Contract::AskAndPlan, Scenario::WrongStreamResponse)
            | (Contract::AskAndPlan, Scenario::DuplicateResponseAfterReconnect)
            | (Contract::AskAndPlan, Scenario::SequentialStaleToolIds)
            | (Contract::AskAndPlan, Scenario::ResponseWithProse)
            | (Contract::AskAndPlan, Scenario::ResponseWithImages)
            | (Contract::AskAndPlan, Scenario::ResponseWhileInputQueued)
            | (Contract::AskAndPlan, Scenario::ResponseRacesInterrupt)
            | (Contract::AskAndPlan, Scenario::ResponseRacesClose)
            | (Contract::AskAndPlan, Scenario::ResponseRacesTransportClose)
            | (Contract::AskAndPlan, Scenario::ResponseRacesReconnect)
            | (Contract::AskAndPlan, Scenario::ResponseRacesHostRestart) => {
                // Cross-kind isolation needs only the routing/order cells. A
                // same-request client conflict and lifecycle-boundary races
                // are properties of each concrete typed contract, not a new
                // mixed-kind behavior.
                if matches!(
                    self.scenario,
                    Scenario::TwoAgentsForwardResponseOrder
                        | Scenario::TwoAgentsReverseResponseOrder
                        | Scenario::TwoAgentsConcurrentResponses
                        | Scenario::WrongStreamResponse
                ) {
                    InteractionConcurrencyApplicability::Required
                } else {
                    InteractionConcurrencyApplicability::ExplicitlyNotApplicable(
                        "mixed-kind isolation adds no distinct lifecycle-boundary contract",
                    )
                }
            }
            _ => InteractionConcurrencyApplicability::Required,
        }
    }

    pub fn required(contract: InteractionConcurrencyContract) -> Vec<Self> {
        InteractionConcurrencyScenario::ALL
            .into_iter()
            .map(|scenario| Self { contract, scenario })
            .filter(|cell| {
                matches!(
                    cell.applicability(),
                    InteractionConcurrencyApplicability::Required
                )
            })
            .collect()
    }

    pub fn required_for(
        contract: InteractionConcurrencyContract,
        capabilities: &BackendCapabilities,
    ) -> Vec<Self> {
        Self::required(contract)
            .into_iter()
            .filter(|cell| match cell.scenario {
                InteractionConcurrencyScenario::ResponseRacesInterrupt => {
                    capabilities.contains(BackendCapability::Interrupt)
                }
                InteractionConcurrencyScenario::ResponseRacesHostRestart => {
                    capabilities.contains(BackendCapability::ResumeSession)
                }
                InteractionConcurrencyScenario::ResponseWithImages => {
                    capabilities.contains(BackendCapability::ImageInput)
                }
                _ => true,
            })
            .collect()
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ToolInputVariant {
        ForegroundCommand,
        BackgroundCommand,
        AskSingleChoice,
        AskMultipleChoice,
        AskFreeText,
        AskMultipleQuestions,
        PlanApproval,
        AgentSpawn,
        SendAgentMessage,
        AwaitAgents,
        BackgroundSubagent,
    }
}

impl ToolInputVariant {
    pub fn tool(self) -> SpecialToolContract {
        match self {
            Self::ForegroundCommand => SpecialToolContract::ForegroundCommand,
            Self::BackgroundCommand => SpecialToolContract::BackgroundCommand,
            Self::AskSingleChoice
            | Self::AskMultipleChoice
            | Self::AskFreeText
            | Self::AskMultipleQuestions => SpecialToolContract::AskUserQuestion,
            Self::PlanApproval => SpecialToolContract::ExitPlanMode,
            Self::AgentSpawn => SpecialToolContract::AgentSpawn,
            Self::SendAgentMessage => SpecialToolContract::SendAgentMessage,
            Self::AwaitAgents => SpecialToolContract::AwaitAgents,
            Self::BackgroundSubagent => SpecialToolContract::BackgroundSubagent,
        }
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum ToolResultVariant {
        Pending,
        Succeeds,
        Fails,
        UserValidResponse,
        UserMultiSelectResponse,
        UserEmptyResponse,
        UserWhitespaceResponse,
        UserWrongToolIdResponse,
        UserStaleResponse,
        PlanApprove,
        PlanReject,
        PlanRejectWithFeedback,
        PlanApproveWithFeedback,
        PlanApproveWithWhitespaceFeedback,
        PlanStaleResponse,
        PlanWrongToolIdResponse,
        AwaitPending,
        AwaitCompletes,
        AwaitIncludesFailure,
        AwaitTimesOut,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AmbientActivityOutcome {
    NotPresent,
    RemainsRunning,
    CompletesBeforeStimulus,
    CompletesDuringStimulus,
    CompletesAfterStimulus,
    FailsBeforeStimulus,
    FailsDuringStimulus,
    FailsAfterStimulus,
    StoppedByBoundary,
    FailedByBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConformanceCoverageCell {
    pub tool: SpecialToolContract,
    pub input: ToolInputVariant,
    pub result: ToolResultVariant,
    pub activity: ActivityCondition,
    pub from: ContractState,
    pub stimulus: ContractStimulus,
    pub outcome: ContractOutcome,
    pub ambient_outcome: AmbientActivityOutcome,
}

impl ConformanceCoverageCell {
    pub fn new(
        tool: SpecialToolContract,
        input: ToolInputVariant,
        result: ToolResultVariant,
        activity: ActivityCondition,
        from: ContractState,
        stimulus: ContractStimulus,
        outcome: ContractOutcome,
    ) -> Self {
        Self {
            tool,
            input,
            result,
            activity,
            from,
            stimulus,
            outcome,
            ambient_outcome: ambient_activity_outcome(activity, stimulus),
        }
    }

    pub fn required() -> Vec<Self> {
        let mut cells = Vec::new();
        for tool in SpecialToolContract::ALL {
            for activity in ActivityCondition::ALL {
                for input in ToolInputVariant::ALL {
                    for result in ToolResultVariant::ALL {
                        for from in ContractState::ALL {
                            for stimulus in ContractStimulus::ALL {
                                if let ContractTransitionApplicability::Required(outcome) =
                                    tool.transition_applicability(input, result, from, stimulus)
                                {
                                    cells.push(Self::new(
                                        tool, input, result, activity, from, stimulus, outcome,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        cells
    }

    pub fn required_for(capabilities: &BackendCapabilities) -> Vec<Self> {
        Self::required()
            .into_iter()
            .filter(|cell| {
                cell.tool.supported_by(capabilities)
                    && cell.activity.supported_by(capabilities)
                    && cell.stimulus.supported_by(capabilities)
            })
            .collect()
    }
}

fn ambient_activity_outcome(
    activity: ActivityCondition,
    stimulus: ContractStimulus,
) -> AmbientActivityOutcome {
    use ActivityCondition as Activity;
    use AmbientActivityOutcome as Outcome;
    use ContractStimulus as Stimulus;

    match activity {
        Activity::ForegroundOnly => Outcome::NotPresent,
        Activity::OneBackgroundTaskRunning
        | Activity::MultipleBackgroundTasksRunning
        | Activity::OneBackgroundSubagentRunning
        | Activity::MultipleBackgroundSubagentsRunning
        | Activity::MixedBackgroundWorkRunning => match stimulus {
            Stimulus::Close => Outcome::StoppedByBoundary,
            Stimulus::HostRestartResume => Outcome::StoppedByBoundary,
            Stimulus::TransportClosed => Outcome::FailedByBoundary,
            _ => Outcome::RemainsRunning,
        },
        Activity::BackgroundCompletesBeforeStimulus
        | Activity::BackgroundSubagentCompletesBeforeStimulus
        | Activity::MixedBackgroundWorkCompletesBeforeStimulus => Outcome::CompletesBeforeStimulus,
        Activity::BackgroundCompletesDuringStimulus
        | Activity::BackgroundSubagentCompletesDuringStimulus
        | Activity::MixedBackgroundWorkCompletesDuringStimulus => Outcome::CompletesDuringStimulus,
        Activity::BackgroundCompletesAfterStimulus
        | Activity::BackgroundSubagentCompletesAfterStimulus
        | Activity::MixedBackgroundWorkCompletesAfterStimulus => match stimulus {
            Stimulus::Close => Outcome::StoppedByBoundary,
            Stimulus::HostRestartResume => Outcome::StoppedByBoundary,
            Stimulus::TransportClosed => Outcome::FailedByBoundary,
            _ => Outcome::CompletesAfterStimulus,
        },
        Activity::BackgroundFailsBeforeStimulus
        | Activity::BackgroundSubagentFailsBeforeStimulus
        | Activity::MixedBackgroundWorkFailsBeforeStimulus => Outcome::FailsBeforeStimulus,
        Activity::BackgroundFailsDuringStimulus
        | Activity::BackgroundSubagentFailsDuringStimulus
        | Activity::MixedBackgroundWorkFailsDuringStimulus => Outcome::FailsDuringStimulus,
        Activity::BackgroundFailsAfterStimulus
        | Activity::BackgroundSubagentFailsAfterStimulus
        | Activity::MixedBackgroundWorkFailsAfterStimulus => match stimulus {
            Stimulus::Close => Outcome::StoppedByBoundary,
            Stimulus::HostRestartResume => Outcome::StoppedByBoundary,
            Stimulus::TransportClosed => Outcome::FailedByBoundary,
            _ => Outcome::FailsAfterStimulus,
        },
    }
}

impl ContractStimulus {
    fn supported_by(self, capabilities: &BackendCapabilities) -> bool {
        match self {
            Self::Interrupt => capabilities.contains(BackendCapability::Interrupt),
            Self::HostRestartResume => capabilities.contains(BackendCapability::ResumeSession),
            Self::Fork => capabilities.contains(BackendCapability::ForkSession),
            _ => true,
        }
    }
}

const WRONG_TOOL_VARIANT: &str = "variant belongs to a different special-tool contract";
const STATE_NOT_USED: &str = "tool contract never occupies this lifecycle state";
const STIMULUS_NOT_VALID: &str = "stimulus is not valid in this lifecycle state";
const VARIANT_STIMULUS_MISMATCH: &str = "stimulus does not apply to this input/result variant";

impl SpecialToolContract {
    pub fn transition_applicability(
        self,
        input: ToolInputVariant,
        result: ToolResultVariant,
        state: ContractState,
        stimulus: ContractStimulus,
    ) -> ContractTransitionApplicability {
        if input.tool() != self {
            return ContractTransitionApplicability::ExplicitlyNotApplicable(WRONG_TOOL_VARIANT);
        }
        if !result.supported_by(self) {
            return ContractTransitionApplicability::ExplicitlyNotApplicable(WRONG_TOOL_VARIANT);
        }

        match self {
            Self::AskUserQuestion | Self::ExitPlanMode => {
                user_rendezvous_applicability(input, result, state, stimulus)
            }
            Self::ForegroundCommand => command_applicability(result, state, stimulus, false),
            Self::BackgroundCommand => command_applicability(result, state, stimulus, true),
            Self::AgentSpawn => foreground_child_applicability(result, state, stimulus),
            Self::SendAgentMessage => child_message_applicability(result, state, stimulus),
            Self::AwaitAgents => child_await_applicability(result, state, stimulus),
            Self::BackgroundSubagent => background_child_applicability(result, state, stimulus),
        }
    }
}

impl ToolResultVariant {
    fn supported_by(self, tool: SpecialToolContract) -> bool {
        match tool {
            SpecialToolContract::ForegroundCommand
            | SpecialToolContract::BackgroundCommand
            | SpecialToolContract::AgentSpawn
            | SpecialToolContract::SendAgentMessage
            | SpecialToolContract::BackgroundSubagent => {
                matches!(self, Self::Pending | Self::Succeeds | Self::Fails)
            }
            SpecialToolContract::AskUserQuestion => {
                matches!(
                    self,
                    Self::Pending
                        | Self::UserValidResponse
                        | Self::UserMultiSelectResponse
                        | Self::UserEmptyResponse
                        | Self::UserWhitespaceResponse
                        | Self::UserWrongToolIdResponse
                        | Self::UserStaleResponse
                )
            }
            SpecialToolContract::ExitPlanMode => matches!(
                self,
                Self::Pending
                    | Self::PlanApprove
                    | Self::PlanReject
                    | Self::PlanRejectWithFeedback
                    | Self::PlanApproveWithFeedback
                    | Self::PlanApproveWithWhitespaceFeedback
                    | Self::PlanStaleResponse
                    | Self::PlanWrongToolIdResponse
            ),
            SpecialToolContract::AwaitAgents => matches!(
                self,
                Self::AwaitPending
                    | Self::AwaitCompletes
                    | Self::AwaitIncludesFailure
                    | Self::AwaitTimesOut
            ),
        }
    }
}

fn required(outcome: ContractOutcome) -> ContractTransitionApplicability {
    ContractTransitionApplicability::Required(outcome)
}

fn not_applicable(reason: &'static str) -> ContractTransitionApplicability {
    ContractTransitionApplicability::ExplicitlyNotApplicable(reason)
}

fn user_rendezvous_applicability(
    input: ToolInputVariant,
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
) -> ContractTransitionApplicability {
    use ContractState as State;
    use ContractStimulus as Stimulus;
    use ToolResultVariant as Result;

    let ask = input.tool() == SpecialToolContract::AskUserQuestion;
    if result == Result::UserMultiSelectResponse && input != ToolInputVariant::AskMultipleChoice {
        return not_applicable(VARIANT_STIMULUS_MISMATCH);
    }
    let valid_response = if ask {
        matches!(
            result,
            Result::UserValidResponse | Result::UserMultiSelectResponse
        )
    } else {
        matches!(
            result,
            Result::PlanApprove | Result::PlanReject | Result::PlanRejectWithFeedback
        )
    };
    let invalid_response = if ask {
        matches!(
            result,
            Result::UserEmptyResponse
                | Result::UserWhitespaceResponse
                | Result::UserWrongToolIdResponse
        )
    } else {
        matches!(
            result,
            Result::PlanWrongToolIdResponse
                | Result::PlanApproveWithFeedback
                | Result::PlanApproveWithWhitespaceFeedback
        )
    };
    let stale_response = if ask {
        result == Result::UserStaleResponse
    } else {
        result == Result::PlanStaleResponse
    };
    let pending = result == Result::Pending;

    match state {
        State::Requested => match stimulus {
            Stimulus::ProviderAccepts if pending => required(ContractOutcome::WaitingForUser),
            Stimulus::ProviderAccepts => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(STIMULUS_NOT_VALID),
        },
        State::WaitingForUser => match stimulus {
            Stimulus::UserResponds if valid_response => required(ContractOutcome::Completed),
            Stimulus::UserResponds if invalid_response => {
                required(ContractOutcome::RejectedVisiblyAndRemainsWaiting)
            }
            Stimulus::Interrupt if pending => required(ContractOutcome::Cancelled),
            Stimulus::Close if pending => required(ContractOutcome::Closed),
            Stimulus::DisconnectReconnect if pending => required(ContractOutcome::RemainsWaiting),
            Stimulus::HostRestartResume if pending => {
                required(ContractOutcome::PendingInteractionExpiredVisibly)
            }
            Stimulus::Fork if pending => required(ContractOutcome::PendingInteractionNotCopied),
            Stimulus::TransportClosed if pending => required(ContractOutcome::FailedVisibly),
            Stimulus::TimeoutExpires if pending => required(ContractOutcome::RemainsWaiting),
            Stimulus::UserResponds => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::ProviderAccepts
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(if stale_response {
                VARIANT_STIMULUS_MISMATCH
            } else {
                STIMULUS_NOT_VALID
            }),
        },
        State::Running => match stimulus {
            Stimulus::ProviderAccepts
            | Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(STATE_NOT_USED),
        },
        State::Terminal => match stimulus {
            Stimulus::UserRespondsAgain if stale_response => {
                required(ContractOutcome::RejectedVisiblyNotConsumed)
            }
            Stimulus::UserRespondsAgain if ask && result == Result::UserValidResponse => {
                required(ContractOutcome::NewInputOrVisibleRejection)
            }
            Stimulus::ProviderAccepts
            | Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(STIMULUS_NOT_VALID),
        },
    }
}

fn command_applicability(
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
    background: bool,
) -> ContractTransitionApplicability {
    use ContractState as State;
    use ContractStimulus as Stimulus;
    use ToolResultVariant as Result;

    match state {
        State::Requested => match stimulus {
            Stimulus::ProviderAccepts if result == Result::Pending => {
                required(ContractOutcome::Running)
            }
            Stimulus::ProviderAccepts => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(STIMULUS_NOT_VALID),
        },
        State::Running => match stimulus {
            Stimulus::ToolCompletes if result == Result::Succeeds => {
                required(ContractOutcome::Completed)
            }
            Stimulus::ToolFails if result == Result::Fails => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::Interrupt if result == Result::Pending => required(if background {
                ContractOutcome::RemainsRunning
            } else {
                ContractOutcome::StoppedByBoundary
            }),
            Stimulus::Close if result == Result::Pending => required(ContractOutcome::Closed),
            Stimulus::DisconnectReconnect | Stimulus::TimeoutExpires
                if result == Result::Pending =>
            {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::Fork if result == Result::Pending => {
                required(ContractOutcome::PendingInteractionNotCopied)
            }
            Stimulus::HostRestartResume if result == Result::Pending => {
                required(ContractOutcome::StoppedByBoundary)
            }
            Stimulus::TransportClosed if result == Result::Pending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::ToolCompletes | Stimulus::ToolFails => {
                not_applicable(VARIANT_STIMULUS_MISMATCH)
            }
            Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires
            | Stimulus::Fork => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::ProviderAccepts
            | Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails => not_applicable(STIMULUS_NOT_VALID),
        },
        State::WaitingForUser | State::Terminal => classify_unused_state(stimulus),
    }
}

fn foreground_child_applicability(
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
) -> ContractTransitionApplicability {
    child_spawn_applicability(result, state, stimulus, false)
}

fn background_child_applicability(
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
) -> ContractTransitionApplicability {
    child_spawn_applicability(result, state, stimulus, true)
}

fn child_spawn_applicability(
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
    background: bool,
) -> ContractTransitionApplicability {
    use ContractState as State;
    use ContractStimulus as Stimulus;
    use ToolResultVariant as Result;

    let succeeds = result == Result::Succeeds;
    match state {
        State::Requested => match stimulus {
            Stimulus::ProviderAccepts if result == Result::Pending => {
                required(ContractOutcome::Running)
            }
            Stimulus::ProviderAccepts => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(STIMULUS_NOT_VALID),
        },
        State::Running => match stimulus {
            Stimulus::ChildCompletes if succeeds => required(ContractOutcome::Completed),
            Stimulus::ChildFails if result == Result::Fails => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::ChildCompletes | Stimulus::ChildFails => {
                not_applicable(VARIANT_STIMULUS_MISMATCH)
            }
            Stimulus::Interrupt if background && result == Result::Pending => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::Interrupt if result == Result::Pending => {
                required(ContractOutcome::Cancelled)
            }
            Stimulus::Close if result == Result::Pending => required(ContractOutcome::Closed),
            Stimulus::DisconnectReconnect if result == Result::Pending => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::HostRestartResume if !background && result == Result::Pending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::HostRestartResume if background && result == Result::Pending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::TransportClosed if result == Result::Pending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::TimeoutExpires if result == Result::Pending => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::Fork if result == Result::Pending => {
                required(ContractOutcome::PendingInteractionNotCopied)
            }
            Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires
            | Stimulus::Fork => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::ProviderAccepts
            | Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails => not_applicable(STIMULUS_NOT_VALID),
        },
        State::WaitingForUser | State::Terminal => classify_unused_state(stimulus),
    }
}

fn child_message_applicability(
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
) -> ContractTransitionApplicability {
    use ContractState as State;
    use ContractStimulus as Stimulus;
    use ToolResultVariant as Result;

    match state {
        State::Requested => match stimulus {
            Stimulus::ProviderAccepts if result == Result::Succeeds => {
                required(ContractOutcome::Completed)
            }
            Stimulus::ChildFails if result == Result::Fails => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::Interrupt if result == Result::Pending => {
                required(ContractOutcome::Cancelled)
            }
            Stimulus::Close if result == Result::Pending => required(ContractOutcome::Closed),
            Stimulus::DisconnectReconnect if result == Result::Pending => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::HostRestartResume if result == Result::Pending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::Fork if result == Result::Pending => {
                required(ContractOutcome::PendingInteractionNotCopied)
            }
            Stimulus::TransportClosed if result == Result::Pending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::TimeoutExpires if result == Result::Pending => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::ProviderAccepts
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes => not_applicable(STIMULUS_NOT_VALID),
        },
        State::WaitingForUser | State::Running | State::Terminal => classify_unused_state(stimulus),
    }
}

fn child_await_applicability(
    result: ToolResultVariant,
    state: ContractState,
    stimulus: ContractStimulus,
) -> ContractTransitionApplicability {
    use ContractState as State;
    use ContractStimulus as Stimulus;
    use ToolResultVariant as Result;

    match state {
        State::Requested => match stimulus {
            Stimulus::ProviderAccepts if result == Result::AwaitPending => {
                required(ContractOutcome::Running)
            }
            Stimulus::ProviderAccepts => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails
            | Stimulus::ChildCompletes
            | Stimulus::ChildFails
            | Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed
            | Stimulus::TimeoutExpires => not_applicable(STIMULUS_NOT_VALID),
        },
        State::Running => match stimulus {
            Stimulus::ChildCompletes if result == Result::AwaitCompletes => {
                required(ContractOutcome::Completed)
            }
            Stimulus::ChildFails if result == Result::AwaitIncludesFailure => {
                required(ContractOutcome::Completed)
            }
            Stimulus::TimeoutExpires if result == Result::AwaitTimesOut => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::ChildCompletes | Stimulus::ChildFails | Stimulus::TimeoutExpires => {
                not_applicable(VARIANT_STIMULUS_MISMATCH)
            }
            Stimulus::Interrupt if result == Result::AwaitPending => {
                required(ContractOutcome::Cancelled)
            }
            Stimulus::Close if result == Result::AwaitPending => required(ContractOutcome::Closed),
            Stimulus::DisconnectReconnect if result == Result::AwaitPending => {
                required(ContractOutcome::RemainsRunning)
            }
            Stimulus::HostRestartResume if result == Result::AwaitPending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::Fork if result == Result::AwaitPending => {
                required(ContractOutcome::PendingInteractionNotCopied)
            }
            Stimulus::TransportClosed if result == Result::AwaitPending => {
                required(ContractOutcome::FailedVisibly)
            }
            Stimulus::Interrupt
            | Stimulus::Close
            | Stimulus::DisconnectReconnect
            | Stimulus::HostRestartResume
            | Stimulus::Fork
            | Stimulus::TransportClosed => not_applicable(VARIANT_STIMULUS_MISMATCH),
            Stimulus::ProviderAccepts
            | Stimulus::UserResponds
            | Stimulus::UserRespondsAgain
            | Stimulus::ToolCompletes
            | Stimulus::ToolFails => not_applicable(STIMULUS_NOT_VALID),
        },
        State::WaitingForUser | State::Terminal => classify_unused_state(stimulus),
    }
}

fn classify_unused_state(stimulus: ContractStimulus) -> ContractTransitionApplicability {
    match stimulus {
        ContractStimulus::ProviderAccepts
        | ContractStimulus::UserResponds
        | ContractStimulus::UserRespondsAgain
        | ContractStimulus::ToolCompletes
        | ContractStimulus::ToolFails
        | ContractStimulus::ChildCompletes
        | ContractStimulus::ChildFails
        | ContractStimulus::Interrupt
        | ContractStimulus::Close
        | ContractStimulus::DisconnectReconnect
        | ContractStimulus::HostRestartResume
        | ContractStimulus::Fork
        | ContractStimulus::TransportClosed
        | ContractStimulus::TimeoutExpires => not_applicable(STATE_NOT_USED),
    }
}

exhaustive_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum TraceInvariant {
        ValidLifecycleOrder,
        StableStreamIdentity,
        UniqueTerminalEvents,
        NoEventsAfterQuiescence,
        InputConsumedExactlyOnce,
        ToolCompletedExactlyOnce,
        BackgroundTaskTerminalExactlyOnce,
        NoTerminalToRunningRegression,
        NoPendingInputAtShutdown,
        NoOpenToolAtShutdown,
        NoOrphanedProcessAtShutdown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceCoverageError {
    pub message: String,
}

impl std::fmt::Display for ConformanceCoverageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConformanceCoverageError {}

#[derive(Debug)]
pub struct ConformanceCoverageLedger {
    required: BTreeSet<ConformanceCoverageCell>,
    executed: BTreeSet<ConformanceCoverageCell>,
}

impl Default for ConformanceCoverageLedger {
    fn default() -> Self {
        Self {
            required: ConformanceCoverageCell::required().into_iter().collect(),
            executed: BTreeSet::new(),
        }
    }
}

impl ConformanceCoverageLedger {
    pub fn for_capabilities(capabilities: &BackendCapabilities) -> Self {
        Self {
            required: ConformanceCoverageCell::required_for(capabilities)
                .into_iter()
                .collect(),
            executed: BTreeSet::new(),
        }
    }

    pub fn record(
        &mut self,
        cell: ConformanceCoverageCell,
    ) -> Result<(), ConformanceCoverageError> {
        if !self.required.contains(&cell) {
            return Err(ConformanceCoverageError {
                message: format!("recorded non-contract conformance cell {cell:?}"),
            });
        }
        if !self.executed.insert(cell) {
            return Err(ConformanceCoverageError {
                message: format!("conformance cell executed twice {cell:?}"),
            });
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), ConformanceCoverageError> {
        let missing: Vec<_> = self.required.difference(&self.executed).copied().collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(ConformanceCoverageError {
            message: format!(
                "{} required conformance transitions did not execute: {missing:?}",
                missing.len()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_generic_request_has_an_explicit_live_capability() {
        let requests = [
            ToolRequestType::ModifyFile {
                file_path: String::new(),
                before: String::new(),
                after: String::new(),
            },
            ToolRequestType::ReadFiles { file_paths: vec![] },
            ToolRequestType::SearchTypes {
                language: String::new(),
                workspace_root: String::new(),
                type_name: String::new(),
            },
            ToolRequestType::GetTypeDocs {
                language: String::new(),
                workspace_root: String::new(),
                type_path: String::new(),
            },
            ToolRequestType::GenerateImage { prompt: None },
            ToolRequestType::WebSearch {
                query: String::new(),
            },
            ToolRequestType::ViewImage {
                path: String::new(),
            },
            ToolRequestType::Sleep { duration_ms: 0 },
            ToolRequestType::Other {
                args: serde_json::Value::Null,
            },
        ];
        let contracts = requests
            .iter()
            .map(|request| {
                assert_eq!(
                    ToolContractClass::for_request(request),
                    ToolContractClass::Generic
                );
                GenericToolContract::for_request(request).expect("generic request classification")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(contracts, GenericToolContract::ALL.into_iter().collect());
        let capabilities = GenericToolContract::ALL
            .into_iter()
            .map(GenericToolContract::required_capability)
            .collect::<BTreeSet<_>>();
        assert_eq!(capabilities.len(), GenericToolContract::ALL.len());
    }

    #[test]
    fn generic_tool_lifecycle_matrix_enumerates_each_meaningful_cell_once() {
        let capabilities = BackendCapabilities::new(
            GenericToolContract::ALL
                .into_iter()
                .map(GenericToolContract::required_capability)
                .chain([
                    BackendCapability::Interrupt,
                    BackendCapability::ResumeSession,
                    BackendCapability::ForkSession,
                ]),
        );
        let classified = GenericToolLifecycleCell::classified_for(&capabilities);
        let required = GenericToolLifecycleCell::required_for(&capabilities);
        let canonical = GenericToolContract::ALL.len()
            * GenericToolLifecyclePhase::ALL.len()
            * GenericToolLifecycleBoundary::ALL.len()
            * ActivityCondition::ALL.len();
        let expected = canonical + 5 + 2 + 7;
        assert_eq!(classified.len(), expected);
        assert_eq!(
            classified.into_iter().collect::<BTreeSet<_>>().len(),
            expected
        );
        let required_len = required.len();
        assert_eq!(
            required.iter().copied().collect::<BTreeSet<_>>().len(),
            required_len
        );
        assert!(required_len < expected);
        assert!(required.iter().any(|cell| {
            cell.timing == GenericToolBoundaryTiming::BeforeRequest
                && cell.boundary == GenericToolLifecycleBoundary::TransportClosedResume
        }));
        assert!(required.iter().any(|cell| {
            cell.multiplicity == GenericToolMultiplicity::HeterogeneousTwo
                && cell.relation == GenericToolCallRelation::IndependentConcurrent
        }));
    }

    #[test]
    fn retry_lifecycle_matrix_classifies_every_real_transport_boundary() {
        let capabilities = BackendCapabilities::new([
            BackendCapability::RetryTelemetry,
            BackendCapability::Interrupt,
        ]);
        let required = RetryLifecycleCell::required_for(&capabilities);
        let explicitly_not_applicable_boundaries = 3;
        let expected = RetryFailureSource::ALL.len()
            * (RetryBoundary::ALL.len() - explicitly_not_applicable_boundaries);
        assert_eq!(required.len(), expected);
        assert_eq!(
            required.into_iter().collect::<BTreeSet<_>>().len(),
            expected
        );

        let no_interrupt = RetryLifecycleCell::required_for(&BackendCapabilities::new([
            BackendCapability::RetryTelemetry,
        ]));
        assert_eq!(
            no_interrupt.len(),
            RetryFailureSource::ALL.len()
                * (RetryBoundary::ALL.len() - explicitly_not_applicable_boundaries - 1)
        );
        assert!(
            no_interrupt
                .iter()
                .all(|cell| cell.boundary != RetryBoundary::Interrupt)
        );

        let all_ambient = BackendCapabilities::new([
            BackendCapability::RetryTelemetry,
            BackendCapability::Interrupt,
            BackendCapability::BackgroundTasks,
            BackendCapability::BackgroundSubagents,
        ]);
        let required = RetryLifecycleCell::required_for(&all_ambient);
        let expected = RetryFailureSource::ALL.len()
            * (RetryBoundary::ALL.len() - explicitly_not_applicable_boundaries)
            * ActivityCondition::ALL.len();
        assert_eq!(required.len(), expected);
        assert_eq!(
            required.into_iter().collect::<BTreeSet<_>>().len(),
            expected
        );
    }

    #[test]
    fn request_usage_lifecycle_requires_each_reachable_shape() {
        assert!(
            RequestUsageLifecycleCell::required_for(&BackendCapabilities::default()).is_empty()
        );
        let base = BackendCapabilities::from([BackendCapability::ModelRequestUsageReported]);
        assert_eq!(
            RequestUsageLifecycleCell::required_for(&base),
            vec![
                RequestUsageLifecycleCell::Plain,
                RequestUsageLifecycleCell::ToolLoop,
                RequestUsageLifecycleCell::MultiTool,
            ]
        );
        let all = BackendCapabilities::from([
            BackendCapability::ModelRequestUsageReported,
            BackendCapability::UserQuestionRequests,
            BackendCapability::RetryTelemetry,
        ]);
        assert_eq!(
            RequestUsageLifecycleCell::required_for(&all),
            RequestUsageLifecycleCell::ALL
        );
    }

    #[test]
    fn matrix_includes_user_response_while_background_work_runs() {
        let cells = ConformanceCoverageCell::required();
        for (tool, input, result) in [
            (
                SpecialToolContract::AskUserQuestion,
                ToolInputVariant::AskSingleChoice,
                ToolResultVariant::UserValidResponse,
            ),
            (
                SpecialToolContract::ExitPlanMode,
                ToolInputVariant::PlanApproval,
                ToolResultVariant::PlanApprove,
            ),
        ] {
            assert!(cells.contains(&ConformanceCoverageCell {
                tool,
                input,
                result,
                activity: ActivityCondition::OneBackgroundTaskRunning,
                from: ContractState::WaitingForUser,
                stimulus: ContractStimulus::UserResponds,
                outcome: ContractOutcome::Completed,
                ambient_outcome: AmbientActivityOutcome::RemainsRunning,
            }));
        }
    }

    #[test]
    fn every_tool_variant_state_stimulus_combination_is_classified() {
        let mut required = 0usize;
        let mut not_applicable = 0usize;
        for tool in SpecialToolContract::ALL {
            for input in ToolInputVariant::ALL {
                for result in ToolResultVariant::ALL {
                    for state in ContractState::ALL {
                        for stimulus in ContractStimulus::ALL {
                            match tool.transition_applicability(input, result, state, stimulus) {
                                ContractTransitionApplicability::Required(_) => required += 1,
                                ContractTransitionApplicability::ExplicitlyNotApplicable(
                                    reason,
                                ) => {
                                    assert!(!reason.trim().is_empty());
                                    not_applicable += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(required > 0);
        assert!(not_applicable > 0);
        assert_eq!(
            required + not_applicable,
            SpecialToolContract::ALL.len()
                * ToolInputVariant::ALL.len()
                * ToolResultVariant::ALL.len()
                * ContractState::ALL.len()
                * ContractStimulus::ALL.len()
        );
    }

    #[test]
    fn every_declared_variant_has_required_coverage() {
        let cells = ConformanceCoverageCell::required();
        for input in ToolInputVariant::ALL {
            assert!(
                cells.iter().any(|cell| cell.input == input),
                "input {input:?} has no required coverage"
            );
        }
        for result in ToolResultVariant::ALL {
            assert!(cells.iter().any(|cell| cell.result == result));
        }
    }

    #[test]
    fn required_cells_are_generated_uniquely() {
        let cells = ConformanceCoverageCell::required();
        let unique: BTreeSet<_> = cells.iter().copied().collect();
        assert_eq!(cells.len(), unique.len());
    }

    #[test]
    fn agent_control_coverage_declares_every_independent_dimension() {
        let cells = AgentControlCoverageCell::required();
        for tool in [
            SpecialToolContract::SendAgentMessage,
            SpecialToolContract::AwaitAgents,
        ] {
            assert!(cells.iter().any(|cell| cell.tool == tool));
        }
        for relation in AgentControlTargetRelation::ALL {
            assert!(cells.iter().any(|cell| cell.relation == relation));
        }
        for authorization in AgentControlAuthorizationSet::ALL {
            assert!(cells.iter().any(|cell| cell.authorization == authorization));
        }
        for multiplicity in AgentControlRequestMultiplicity::ALL {
            assert!(cells.iter().any(|cell| cell.multiplicity == multiplicity));
        }
        for boundary_race in AgentControlBoundaryRace::ALL {
            assert!(cells.iter().any(|cell| cell.boundary_race == boundary_race));
        }
    }

    #[test]
    fn agent_control_coverage_is_the_valid_cartesian_product() {
        let required = AgentControlCoverageCell::required();
        for tool in [
            SpecialToolContract::SendAgentMessage,
            SpecialToolContract::AwaitAgents,
        ] {
            for relation in AgentControlTargetRelation::ALL {
                for authorization in AgentControlAuthorizationSet::ALL {
                    for multiplicity in AgentControlRequestMultiplicity::ALL {
                        for boundary_race in AgentControlBoundaryRace::ALL {
                            let cell = AgentControlCoverageCell {
                                tool,
                                relation,
                                authorization,
                                multiplicity,
                                boundary_race,
                            };
                            assert_eq!(
                                required.contains(&cell),
                                AgentControlCoverageCell::is_valid_contract_cell(&cell),
                                "agent-control applicability disagreed for {cell:?}"
                            );
                        }
                    }
                }
            }
        }
        let unsupported = BackendCapabilities::default();
        assert!(
            AgentControlCoverageLedger::for_capabilities(&unsupported)
                .required
                .is_empty()
        );
    }

    #[test]
    fn progress_coverage_crosses_modes_and_boundaries() {
        let capabilities = BackendCapabilities::new(BackendCapability::ALL);
        let cells = ProgressCoverageCell::required_for(&capabilities);
        for family in ProgressFamily::ALL {
            for transition in ProgressTransition::ALL {
                assert!(
                    cells
                        .iter()
                        .any(|cell| { cell.family == family && cell.transition == transition }),
                    "missing {family:?} {transition:?} progress cell"
                );
            }
        }
        for mode in [
            ProgressSubagentMode::Foreground,
            ProgressSubagentMode::Background,
        ] {
            for transition in ProgressTransition::ALL {
                assert!(cells.contains(&ProgressCoverageCell {
                    family: ProgressFamily::SubAgent,
                    subagent_mode: mode,
                    transition,
                }));
            }
        }
        assert!(cells.iter().all(|cell| {
            (cell.family == ProgressFamily::SubAgent)
                != (cell.subagent_mode == ProgressSubagentMode::NotApplicable)
        }));
    }

    #[test]
    fn task_update_lifecycle_classifies_every_row_truthfully() {
        let capabilities = BackendCapabilities::new(BackendCapability::ALL);
        let required = TaskUpdateLifecycleCell::required_for(&capabilities);
        for cell in TaskUpdateLifecycleCell::ALL {
            match cell.applicability(&capabilities) {
                TaskUpdateLifecycleApplicability::Required => {
                    assert!(
                        required.contains(&cell),
                        "missing required task row {cell:?}"
                    );
                }
                TaskUpdateLifecycleApplicability::ExplicitlyNotApplicable(reason) => {
                    assert!(!reason.trim().is_empty());
                    assert!(!required.contains(&cell));
                }
            }
        }
        let without_lineage = BackendCapabilities::from([
            BackendCapability::TaskUpdates,
            BackendCapability::Interrupt,
        ]);
        let required = TaskUpdateLifecycleCell::required_for(&without_lineage);
        assert!(!required.contains(&TaskUpdateLifecycleCell::HostRestartResumePersistence));
        assert!(!required.contains(&TaskUpdateLifecycleCell::ForkIsolation));
        assert!(required.contains(&TaskUpdateLifecycleCell::InterruptBoundary));
    }

    #[test]
    fn normalized_turn_coverage_declares_all_shapes_and_boundaries() {
        let capabilities = BackendCapabilities::new(BackendCapability::ALL);
        let ledger = NormalizedTurnCoverageLedger::for_capabilities(&capabilities);
        for shape in NormalizedTurnShape::ALL {
            assert!(
                ledger
                    .required
                    .contains(&NormalizedTurnCoverageCell::Shape(shape))
            );
        }
        for mechanism in PlainTerminationMechanism::ALL {
            for phase in PlainTerminationPhase::ALL {
                assert!(
                    ledger
                        .required
                        .contains(&NormalizedTurnCoverageCell::Termination { mechanism, phase })
                );
            }
        }
    }

    #[test]
    fn enumerated_live_ledgers_reject_missing_cells() {
        let mut sessions =
            EnumeratedCoverageLedger::new(SessionListLifecycleCell::ALL, "session-list lifecycle");
        for cell in SessionListLifecycleCell::ALL {
            sessions.record(cell).unwrap();
        }
        sessions.finish().unwrap();

        let mut customization =
            EnumeratedCoverageLedger::new(LiveCustomizationCell::ALL, "live customization");
        for cell in LiveCustomizationCell::ALL {
            customization.record(cell).unwrap();
        }
        customization.finish().unwrap();
    }

    #[test]
    fn ambient_boundary_outcomes_are_explicit() {
        assert_eq!(
            ambient_activity_outcome(
                ActivityCondition::OneBackgroundTaskRunning,
                ContractStimulus::Close,
            ),
            AmbientActivityOutcome::StoppedByBoundary
        );
        assert_eq!(
            ambient_activity_outcome(
                ActivityCondition::BackgroundCompletesAfterStimulus,
                ContractStimulus::TransportClosed,
            ),
            AmbientActivityOutcome::FailedByBoundary
        );
        assert_eq!(
            ambient_activity_outcome(
                ActivityCondition::BackgroundCompletesAfterStimulus,
                ContractStimulus::UserResponds,
            ),
            AmbientActivityOutcome::CompletesAfterStimulus
        );
    }

    #[test]
    fn await_contract_includes_independent_ambient_activity() {
        let cells = ConformanceCoverageCell::required();
        assert!(cells.iter().any(|cell| {
            cell.tool == SpecialToolContract::AwaitAgents
                && cell.result == ToolResultVariant::AwaitPending
                && cell.from == ContractState::Requested
                && cell.stimulus == ContractStimulus::ProviderAccepts
        }));
        for activity in ActivityCondition::ALL {
            assert!(cells.iter().any(|cell| {
                cell.tool == SpecialToolContract::AwaitAgents && cell.activity == activity
            }));
        }
    }

    #[test]
    fn await_completion_applies_when_any_watched_child_becomes_ready() {
        assert_eq!(
            SpecialToolContract::AwaitAgents.transition_applicability(
                ToolInputVariant::AwaitAgents,
                ToolResultVariant::AwaitCompletes,
                ContractState::Running,
                ContractStimulus::ChildCompletes,
            ),
            ContractTransitionApplicability::Required(ContractOutcome::Completed)
        );
        assert_eq!(
            SpecialToolContract::AwaitAgents.transition_applicability(
                ToolInputVariant::AwaitAgents,
                ToolResultVariant::AwaitIncludesFailure,
                ContractState::Running,
                ContractStimulus::ChildFails,
            ),
            ContractTransitionApplicability::Required(ContractOutcome::Completed)
        );
    }

    #[test]
    fn rendezvous_terminal_timing_applies_to_every_lifecycle_stimulus() {
        let cells = ConformanceCoverageCell::required();
        for tool in [
            SpecialToolContract::AskUserQuestion,
            SpecialToolContract::ExitPlanMode,
        ] {
            assert!(cells.iter().any(|cell| {
                cell.tool == tool
                    && cell.activity == ActivityCondition::BackgroundFailsDuringStimulus
                    && cell.from == ContractState::WaitingForUser
                    && cell.stimulus == ContractStimulus::UserResponds
            }));
            assert!(cells.iter().any(|cell| {
                cell.tool == tool
                    && cell.activity == ActivityCondition::BackgroundFailsDuringStimulus
                    && cell.stimulus == ContractStimulus::Interrupt
            }));
        }
    }

    #[test]
    fn matrix_does_not_generate_impossible_blind_cross_products() {
        let cells = ConformanceCoverageCell::required();
        assert!(!cells.iter().any(|cell| {
            cell.tool == SpecialToolContract::BackgroundCommand
                && cell.stimulus == ContractStimulus::UserResponds
        }));
    }

    #[test]
    fn incomplete_ledger_fails_closed() {
        let error = ConformanceCoverageLedger::default()
            .finish()
            .expect_err("empty qualification cannot pass");
        assert!(error.message.contains("did not execute"));
    }

    #[test]
    fn complete_ledger_passes() {
        let mut ledger = ConformanceCoverageLedger::default();
        for cell in ConformanceCoverageCell::required() {
            ledger.record(cell).expect("record required cell");
        }
        ledger.finish().expect("complete qualification");
    }

    #[test]
    fn input_admission_matrix_is_capability_gated_and_fails_closed() {
        let base = BackendCapabilities::default();
        let base_cells = InputAdmissionCoverageCell::required_for(&base);
        assert!(base_cells.iter().all(|cell| !matches!(
            cell.state,
            InputAdmissionState::UserQuestionWaiting
                | InputAdmissionState::PlanApprovalWaiting
                | InputAdmissionState::McpToolPending
                | InputAdmissionState::GenericOtherPending
                | InputAdmissionState::NativeSpawnPending
                | InputAdmissionState::AgentAwaitPending
                | InputAdmissionState::CompactionPending
                | InputAdmissionState::BackgroundOnlyIdle
                | InputAdmissionState::AgentInitiatedTurn
        )));
        let capabilities = BackendCapabilities::from([
            BackendCapability::UserQuestionRequests,
            BackendCapability::PlanApprovalRequests,
            BackendCapability::BackgroundTasks,
            BackendCapability::BackgroundSubagents,
            BackendCapability::AgentInitiatedTurns,
            BackendCapability::Interrupt,
            BackendCapability::ResumeSession,
            BackendCapability::ForkSession,
            BackendCapability::ImageInput,
            BackendCapability::StartupMcpServers,
            BackendCapability::GenericOtherTool,
            BackendCapability::ForegroundSubagents,
            BackendCapability::AgentControlTools,
            BackendCapability::CompactionReported,
        ]);
        let cells = InputAdmissionCoverageCell::required_for(&capabilities);
        for state in InputAdmissionState::ALL {
            assert!(cells.iter().any(|cell| cell.state == state));
        }
        for action in InputAdmissionAction::ALL {
            assert!(cells.iter().any(|cell| cell.action == action));
        }
        for input in InputAdmissionKind::ALL {
            assert!(cells.iter().any(|cell| cell.input == input));
        }
        for state in InputAdmissionState::ALL {
            for activity in ActivityCondition::ALL
                .into_iter()
                .filter(|activity| activity.supported_by(&capabilities))
            {
                assert!(cells.contains(&InputAdmissionCoverageCell {
                    activity,
                    state,
                    input: InputAdmissionKind::PlainMessage,
                    action: InputAdmissionAction::Admit,
                }));
            }
        }
        for state in InputAdmissionState::ALL {
            for activity in ActivityCondition::ALL
                .into_iter()
                .filter(|activity| activity.supported_by(&capabilities))
            {
                for input in [
                    InputAdmissionKind::AskResponseCurrent,
                    InputAdmissionKind::AskResponseStale,
                    InputAdmissionKind::AskResponseWrongKind,
                    InputAdmissionKind::AskResponseForeignStream,
                    InputAdmissionKind::PlanResponseCurrent,
                    InputAdmissionKind::PlanResponseStale,
                    InputAdmissionKind::PlanResponseWrongKind,
                    InputAdmissionKind::PlanResponseForeignStream,
                ] {
                    assert!(cells.contains(&InputAdmissionCoverageCell {
                        activity,
                        state,
                        input,
                        action: InputAdmissionAction::Admit,
                    }));
                }
            }
        }
        for state in [
            InputAdmissionState::UserQuestionWaiting,
            InputAdmissionState::PlanApprovalWaiting,
        ] {
            for action in [
                InputAdmissionAction::Admit,
                InputAdmissionAction::Enqueue,
                InputAdmissionAction::EditQueued,
                InputAdmissionAction::CancelQueued,
                InputAdmissionAction::SendQueuedNow,
                InputAdmissionAction::InterruptThenSend,
                InputAdmissionAction::ClientReconnect,
                InputAdmissionAction::HostRestart,
                InputAdmissionAction::Fork,
                InputAdmissionAction::TransportClosed,
                InputAdmissionAction::Close,
                InputAdmissionAction::BackgroundTerminal,
                InputAdmissionAction::BackgroundFailureTerminal,
            ] {
                let activity = match action {
                    InputAdmissionAction::BackgroundTerminal => {
                        ActivityCondition::BackgroundCompletesDuringStimulus
                    }
                    InputAdmissionAction::BackgroundFailureTerminal => {
                        ActivityCondition::BackgroundFailsDuringStimulus
                    }
                    _ => ActivityCondition::ForegroundOnly,
                };
                assert!(cells.contains(&InputAdmissionCoverageCell {
                    activity,
                    state,
                    input: InputAdmissionKind::InteractionResponse,
                    action,
                }));
            }
        }
        for state in InputAdmissionState::ALL.into_iter().filter(|state| {
            !matches!(
                state,
                InputAdmissionState::Idle | InputAdmissionState::BackgroundOnlyIdle
            )
        }) {
            for action in [
                InputAdmissionAction::Enqueue,
                InputAdmissionAction::EditQueued,
                InputAdmissionAction::CancelQueued,
                InputAdmissionAction::SendQueuedNow,
                InputAdmissionAction::ClientReconnect,
                InputAdmissionAction::HostRestart,
            ] {
                assert!(cells.contains(&InputAdmissionCoverageCell {
                    activity: ActivityCondition::ForegroundOnly,
                    state,
                    input: InputAdmissionKind::ImageMessage,
                    action,
                }));
            }
        }
        assert!(
            InputAdmissionCoverageLedger::for_capabilities(&capabilities)
                .finish()
                .is_err()
        );
    }
}
