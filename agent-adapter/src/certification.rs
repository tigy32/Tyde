use crate::BackendCapability;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificationTier {
    Deterministic,
    CheapLive,
    StatefulLive,
    SpecializedLive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CertificationCase {
    InitialInputEchoedOnce,
    TypingStarts,
    TypingStartsOnce,
    StreamStarts,
    VisibleDeltaEmitted,
    StreamEnds,
    StreamStartsOnce,
    StreamEndsOnce,
    TypingStopsOnce,
    NoErrorOnSuccessfulTurn,
    TypingStopsAfterStreamEnd,
    LifecycleOrderIsValid,
    StreamIdentityIsStable,
    DeltasReconstructFinalMessage,
    ExactResponseDelivered,
    FollowUpCompletes,
    FollowUpInputEchoedOnce,
    FollowUpUsesDistinctMessage,
    TurnUsagePresent,
    TurnInputTokensPositive,
    TurnOutputTokensPositive,
    TurnTotalConsistent,
    CumulativeUsageGrows,
    RequestUsagePresent,
    RequestSequenceStartsAtOne,
    RequestSequencesContiguous,
    RequestUsageMatchesTurn,
    RequestIdsUnique,
    RequestUsagePositive,
    ContextUsagePresent,
    ContextWindowValid,
    ToolRequestEmitted,
    ToolCompletionEmitted,
    ToolCallIdsCorrelate,
    ToolChangesWorkspace,
    ToolFailureReported,
    ToolTurnReachesIdle,
    InterruptEmitsCancellation,
    InterruptReturnsIdle,
    InterruptStopsCommand,
    FollowUpAfterInterrupt,
    SessionAppearsInList,
    ResumeRemembersHistory,
    ResumeAcceptsFollowUp,
    ResumePreservesWorkspace,
    ForkCreatesDistinctSession,
    ForkPreservesHistory,
    ForkAcceptsInitialPrompt,
    WorkspaceInstructionsObserved,
    HostSteeringObserved,
    SkillDiscovered,
    SkillResultDelivered,
    McpToolDiscovered,
    McpToolCalled,
    McpResultDelivered,
    McpEventsCorrelate,
    ImageEchoPreserved,
    ImageUnderstood,
    ImageResponseStreams,
    NativeSubagentProjected,
    NativeSubagentParentLinked,
    BackgroundWorkKeepsAgentActive,
    BackgroundCompletionResumesParent,
    AgentInitiatedTurnIsDistinct,
    AgentInitiatedResultDelivered,
}

impl CertificationCase {
    pub const ALL: [Self; 65] = [
        Self::InitialInputEchoedOnce,
        Self::TypingStarts,
        Self::TypingStartsOnce,
        Self::StreamStarts,
        Self::VisibleDeltaEmitted,
        Self::StreamEnds,
        Self::StreamStartsOnce,
        Self::StreamEndsOnce,
        Self::TypingStopsOnce,
        Self::NoErrorOnSuccessfulTurn,
        Self::TypingStopsAfterStreamEnd,
        Self::LifecycleOrderIsValid,
        Self::StreamIdentityIsStable,
        Self::DeltasReconstructFinalMessage,
        Self::ExactResponseDelivered,
        Self::FollowUpCompletes,
        Self::FollowUpInputEchoedOnce,
        Self::FollowUpUsesDistinctMessage,
        Self::TurnUsagePresent,
        Self::TurnInputTokensPositive,
        Self::TurnOutputTokensPositive,
        Self::TurnTotalConsistent,
        Self::CumulativeUsageGrows,
        Self::RequestUsagePresent,
        Self::RequestSequenceStartsAtOne,
        Self::RequestSequencesContiguous,
        Self::RequestUsageMatchesTurn,
        Self::RequestIdsUnique,
        Self::RequestUsagePositive,
        Self::ContextUsagePresent,
        Self::ContextWindowValid,
        Self::ToolRequestEmitted,
        Self::ToolCompletionEmitted,
        Self::ToolCallIdsCorrelate,
        Self::ToolChangesWorkspace,
        Self::ToolFailureReported,
        Self::ToolTurnReachesIdle,
        Self::InterruptEmitsCancellation,
        Self::InterruptReturnsIdle,
        Self::InterruptStopsCommand,
        Self::FollowUpAfterInterrupt,
        Self::SessionAppearsInList,
        Self::ResumeRemembersHistory,
        Self::ResumeAcceptsFollowUp,
        Self::ResumePreservesWorkspace,
        Self::ForkCreatesDistinctSession,
        Self::ForkPreservesHistory,
        Self::ForkAcceptsInitialPrompt,
        Self::WorkspaceInstructionsObserved,
        Self::HostSteeringObserved,
        Self::SkillDiscovered,
        Self::SkillResultDelivered,
        Self::McpToolDiscovered,
        Self::McpToolCalled,
        Self::McpResultDelivered,
        Self::McpEventsCorrelate,
        Self::ImageEchoPreserved,
        Self::ImageUnderstood,
        Self::ImageResponseStreams,
        Self::NativeSubagentProjected,
        Self::NativeSubagentParentLinked,
        Self::BackgroundWorkKeepsAgentActive,
        Self::BackgroundCompletionResumesParent,
        Self::AgentInitiatedTurnIsDistinct,
        Self::AgentInitiatedResultDelivered,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::InitialInputEchoedOnce => "initial_input_echoed_once",
            Self::TypingStarts => "typing_starts",
            Self::TypingStartsOnce => "typing_starts_once",
            Self::StreamStarts => "stream_starts",
            Self::VisibleDeltaEmitted => "visible_delta_emitted",
            Self::StreamEnds => "stream_ends",
            Self::StreamStartsOnce => "stream_starts_once",
            Self::StreamEndsOnce => "stream_ends_once",
            Self::TypingStopsOnce => "typing_stops_once",
            Self::NoErrorOnSuccessfulTurn => "no_error_on_successful_turn",
            Self::TypingStopsAfterStreamEnd => "typing_stops_after_stream_end",
            Self::LifecycleOrderIsValid => "lifecycle_order_is_valid",
            Self::StreamIdentityIsStable => "stream_identity_is_stable",
            Self::DeltasReconstructFinalMessage => "deltas_reconstruct_final_message",
            Self::ExactResponseDelivered => "exact_response_delivered",
            Self::FollowUpCompletes => "follow_up_completes",
            Self::FollowUpInputEchoedOnce => "follow_up_input_echoed_once",
            Self::FollowUpUsesDistinctMessage => "follow_up_uses_distinct_message",
            Self::TurnUsagePresent => "turn_usage_present",
            Self::TurnInputTokensPositive => "turn_input_tokens_positive",
            Self::TurnOutputTokensPositive => "turn_output_tokens_positive",
            Self::TurnTotalConsistent => "turn_total_consistent",
            Self::CumulativeUsageGrows => "cumulative_usage_grows",
            Self::RequestUsagePresent => "request_usage_present",
            Self::RequestSequenceStartsAtOne => "request_sequence_starts_at_one",
            Self::RequestSequencesContiguous => "request_sequences_contiguous",
            Self::RequestUsageMatchesTurn => "request_usage_matches_turn",
            Self::RequestIdsUnique => "request_ids_unique",
            Self::RequestUsagePositive => "request_usage_positive",
            Self::ContextUsagePresent => "context_usage_present",
            Self::ContextWindowValid => "context_window_valid",
            Self::ToolRequestEmitted => "tool_request_emitted",
            Self::ToolCompletionEmitted => "tool_completion_emitted",
            Self::ToolCallIdsCorrelate => "tool_call_ids_correlate",
            Self::ToolChangesWorkspace => "tool_changes_workspace",
            Self::ToolFailureReported => "tool_failure_reported",
            Self::ToolTurnReachesIdle => "tool_turn_reaches_idle",
            Self::InterruptEmitsCancellation => "interrupt_emits_cancellation",
            Self::InterruptReturnsIdle => "interrupt_returns_idle",
            Self::InterruptStopsCommand => "interrupt_stops_command",
            Self::FollowUpAfterInterrupt => "follow_up_after_interrupt",
            Self::SessionAppearsInList => "session_appears_in_list",
            Self::ResumeRemembersHistory => "resume_remembers_history",
            Self::ResumeAcceptsFollowUp => "resume_accepts_follow_up",
            Self::ResumePreservesWorkspace => "resume_preserves_workspace",
            Self::ForkCreatesDistinctSession => "fork_creates_distinct_session",
            Self::ForkPreservesHistory => "fork_preserves_history",
            Self::ForkAcceptsInitialPrompt => "fork_accepts_initial_prompt",
            Self::WorkspaceInstructionsObserved => "workspace_instructions_observed",
            Self::HostSteeringObserved => "host_steering_observed",
            Self::SkillDiscovered => "skill_discovered",
            Self::SkillResultDelivered => "skill_result_delivered",
            Self::McpToolDiscovered => "mcp_tool_discovered",
            Self::McpToolCalled => "mcp_tool_called",
            Self::McpResultDelivered => "mcp_result_delivered",
            Self::McpEventsCorrelate => "mcp_events_correlate",
            Self::ImageEchoPreserved => "image_echo_preserved",
            Self::ImageUnderstood => "image_understood",
            Self::ImageResponseStreams => "image_response_streams",
            Self::NativeSubagentProjected => "native_subagent_projected",
            Self::NativeSubagentParentLinked => "native_subagent_parent_linked",
            Self::BackgroundWorkKeepsAgentActive => "background_work_keeps_agent_active",
            Self::BackgroundCompletionResumesParent => "background_completion_resumes_parent",
            Self::AgentInitiatedTurnIsDistinct => "agent_initiated_turn_is_distinct",
            Self::AgentInitiatedResultDelivered => "agent_initiated_result_delivered",
        }
    }

    pub fn tier(self) -> CertificationTier {
        match self {
            Self::NativeSubagentProjected
            | Self::NativeSubagentParentLinked
            | Self::BackgroundWorkKeepsAgentActive
            | Self::BackgroundCompletionResumesParent
            | Self::AgentInitiatedTurnIsDistinct
            | Self::AgentInitiatedResultDelivered => CertificationTier::SpecializedLive,
            Self::FollowUpCompletes
            | Self::FollowUpInputEchoedOnce
            | Self::FollowUpUsesDistinctMessage
            | Self::CumulativeUsageGrows
            | Self::InterruptEmitsCancellation
            | Self::InterruptReturnsIdle
            | Self::InterruptStopsCommand
            | Self::FollowUpAfterInterrupt
            | Self::SessionAppearsInList
            | Self::ResumeRemembersHistory
            | Self::ResumeAcceptsFollowUp
            | Self::ResumePreservesWorkspace
            | Self::ForkCreatesDistinctSession
            | Self::ForkPreservesHistory
            | Self::ForkAcceptsInitialPrompt => CertificationTier::StatefulLive,
            _ => CertificationTier::CheapLive,
        }
    }

    pub fn required_capability(self) -> Option<BackendCapability> {
        match self {
            Self::TurnUsagePresent
            | Self::TurnInputTokensPositive
            | Self::TurnOutputTokensPositive
            | Self::TurnTotalConsistent
            | Self::CumulativeUsageGrows => Some(BackendCapability::TurnUsageReported),
            Self::RequestUsagePresent
            | Self::RequestSequenceStartsAtOne
            | Self::RequestSequencesContiguous
            | Self::RequestUsageMatchesTurn
            | Self::RequestIdsUnique
            | Self::RequestUsagePositive => Some(BackendCapability::ModelRequestUsageReported),
            Self::ContextUsagePresent | Self::ContextWindowValid => {
                Some(BackendCapability::ContextUsageReported)
            }
            Self::InterruptEmitsCancellation
            | Self::InterruptReturnsIdle
            | Self::InterruptStopsCommand
            | Self::FollowUpAfterInterrupt => Some(BackendCapability::Interrupt),
            Self::SessionAppearsInList => Some(BackendCapability::ListSessions),
            Self::ResumeRemembersHistory
            | Self::ResumeAcceptsFollowUp
            | Self::ResumePreservesWorkspace => Some(BackendCapability::ResumeSession),
            Self::ForkCreatesDistinctSession
            | Self::ForkPreservesHistory
            | Self::ForkAcceptsInitialPrompt => Some(BackendCapability::ForkSession),
            Self::WorkspaceInstructionsObserved => Some(BackendCapability::WorkspaceInstructions),
            Self::HostSteeringObserved | Self::SkillDiscovered | Self::SkillResultDelivered => {
                Some(BackendCapability::Customization)
            }
            Self::McpToolDiscovered
            | Self::McpToolCalled
            | Self::McpResultDelivered
            | Self::McpEventsCorrelate => Some(BackendCapability::StartupMcpServers),
            Self::ImageEchoPreserved | Self::ImageUnderstood | Self::ImageResponseStreams => {
                Some(BackendCapability::ImageInput)
            }
            Self::NativeSubagentProjected | Self::NativeSubagentParentLinked => {
                Some(BackendCapability::Subagents)
            }
            Self::BackgroundWorkKeepsAgentActive | Self::BackgroundCompletionResumesParent => {
                Some(BackendCapability::BackgroundTasks)
            }
            Self::AgentInitiatedTurnIsDistinct | Self::AgentInitiatedResultDelivered => {
                Some(BackendCapability::AgentInitiatedTurns)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::CertificationCase;

    #[test]
    fn certification_case_ids_are_unique() {
        let mut ids = HashSet::new();
        for case in CertificationCase::ALL {
            assert!(ids.insert(case.id()), "duplicate case id {}", case.id());
        }
    }
}
