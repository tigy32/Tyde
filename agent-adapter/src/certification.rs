use crate::BackendCapability;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificationTier {
    Deterministic,
    CheapLive,
    StatefulLive,
    SpecializedLive,
}

macro_rules! exhaustive_certification_cases {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum CertificationCase {
            $($variant),+
        }

        impl CertificationCase {
            pub const ALL: [Self; 0usize $(+ exhaustive_certification_cases!(@one $variant))+] = [
                $(Self::$variant),+
            ];
        }
    };
    (@one $variant:ident) => { 1usize };
}

exhaustive_certification_cases! {
    CapabilityTruthMatrix,
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
    RequestUsageAggregatesToTurn,
    RequestIdsUnique,
    RequestUsagePositive,
    RequestUsageLifecycleMatrix,
    ContextUsagePresent,
    ContextWindowValid,
    ContextBreakdownPresent,
    ReasoningDeltaReported,
    TaskUpdateReported,
    TaskUpdateLifecycleMatrix,
    TaskListReplacement,
    TaskListClear,
    WorkflowProgressLifecycle,
    OpaqueToolProgressReported,
    RetryRecoveryMatrix,
    RetryLifecycleMatrix,
    RetryFaultInteractionMatrix,
    ConnectionCardinalityLifecycleMatrix,
    ProgressFamilyLifecycleMatrix,
    ProgressConcurrencyMatrix,
    OrchestrationEventReported,
    OrchestrationPayloadMatrix,
    OrchestrationOutcomeMatrix,
    OrchestrationCorrelationMatrix,
    OrchestrationLifecycleMatrix,
    CompactionObserved,
    CompactionLifecycleMatrix,
    CompactionResumeExactlyOnce,
    TeamContextCompactionContract,
    CompactionCapabilityContract,
    ToolRequestEmitted,
    ToolCompletionEmitted,
    ToolCallIdsCorrelate,
    ToolResponseOwnership,
    ToolChangesWorkspace,
    ToolFailureReported,
    ToolTurnReachesIdle,
    GenericModifyFileContract,
    GenericReadFilesContract,
    GenericSearchTypesContract,
    GenericGetTypeDocsContract,
    GenericGenerateImageContract,
    GenericWebSearchContract,
    GenericViewImageContract,
    GenericSleepContract,
    GenericOtherToolContract,
    GenericToolLifecycleMatrix,
    InterruptEmitsCancellation,
    InterruptReturnsIdle,
    InterruptStopsCommand,
    FollowUpAfterInterrupt,
    InterruptFollowUpSurvivesRequestTimeout,
    InterruptAllowsFollowUpDuringBackgroundWork,
    InterruptPhaseContractMatrix,
    InterruptForkContract,
    ForegroundCommandContractMatrix,
    BackgroundCommandContractMatrix,
    BackgroundProgressLifecycle,
    UserQuestionAnswerDuringBackgroundWork,
    UserQuestionWaitsForAnswer,
    UserQuestionInterrupts,
    UserQuestionCloses,
    UserQuestionSurvivesReconnect,
    UserQuestionNotCopiedToFork,
    UserQuestionExpiresOnHostRestart,
    UserQuestionFailsOnTransportClose,
    UserQuestionResponseEdgeMatrix,
    UserQuestionConcurrencyRaceMatrix,
    PlanApprovalAnswerDuringBackgroundWork,
    PlanApprovalWaitsForDecision,
    PlanApprovalInterrupts,
    PlanApprovalCloses,
    PlanApprovalSurvivesReconnect,
    PlanApprovalNotCopiedToFork,
    PlanApprovalExpiresOnHostRestart,
    PlanApprovalFailsOnTransportClose,
    PlanApprovalResponseEdgeMatrix,
    PlanApprovalConcurrencyRaceMatrix,
    MixedInteractionIsolationMatrix,
    AgentMessageAccepted,
    AgentMessageChildFailureVisible,
    AgentMessageContractMatrix,
    AgentAwaitStarts,
    AgentAwaitCompletesOnChildSuccess,
    AgentAwaitCompletesOnChildFailure,
    AgentAwaitInterrupts,
    AgentAwaitCloses,
    AgentAwaitSurvivesReconnect,
    AgentAwaitExpiresOnHostRestart,
    AgentAwaitNotCopiedToFork,
    AgentAwaitWaits,
    AgentAwaitFailsOnTransportClose,
    AgentAwaitMultiChildContractMatrix,
    AgentControlInvalidEdgeMatrix,
    AgentControlExhaustiveMatrix,
    AgentControlSpawnContract,
    InputAdmissionQueueMatrix,
    NormalizedTurnShapeMatrix,
    PlainTerminationMatrix,
    UsageLifecycleMatrix,
    UsageMetadataConcurrencyMatrix,
    SessionAppearsInList,
    SessionListLifecycleMatrix,
    ResumeRemembersHistory,
    ResumeAcceptsFollowUp,
    ResumePreservesWorkspace,
    ToolMetadataSurvivesReconnectResume,
    ResumeOwnsStartupNotifications,
    StartupLifecycleMatrix,
    StartupForkLifecycleMatrix,
    AuthoritativeResumePreservesConcurrentEvents,
    RepeatedResumeFollowUpExactlyOnce,
    PersistedQueueResumeNoFalseIdle,
    ResumeClosedDispatchTerminalizesOnce,
    ResumeBusyThenQuiescentDispatchesOnce,
    ForkCreatesDistinctSession,
    ForkPreservesHistory,
    ForkAcceptsInitialPrompt,
    SessionSettingsApplied,
    MidTurnSteeringDelivered,
    WorkspaceInstructionsObserved,
    HostSteeringObserved,
    CustomizationSourcesCompose,
    LiveCustomizationMatrix,
    SkillLifecycleMatrix,
    McpToolDiscovered,
    McpToolCalled,
    McpResultDelivered,
    McpEventsCorrelate,
    McpExactlyOnce,
    McpCompletionSuccessful,
    McpToolNamesCorrelate,
    McpCanonicalResultPreserved,
    McpStartupAdversarialContract,
    McpConnectionOwnershipMatrix,
    McpPendingProcessDeathContract,
    McpReconnectContract,
    McpHostRestartResumeContract,
    McpHttpTransportMatrix,
    McpForkContract,
    ImageEchoPreserved,
    ImageUnderstood,
    ImageResponseStreams,
    ImageMultipleAttachmentsPreserved,
    ImageFollowUpPreserved,
    ImageAttachmentContractMatrix,
    ImagePersistenceContractMatrix,
    ReadOnlyAccessEnforced,
    NativeSubagentProjected,
    NativeSubagentParentLinked,
    NativeSubagentUsageIsolated,
    NativeSubagentResumeReplay,
    NativeSubagentForkReplay,
    AgentSpawnContractMatrix,
    BackgroundSubagentContractMatrix,
    BackgroundWorkAllowsForegroundIdle,
    BackgroundWorkIsNotSupervisorStalled,
    BackgroundCompletionReleasesAgent,
    BackgroundCompletionRetiresTrayEntry,
    BackgroundCompletionResumesParent,
    AgentInitiatedTurnIsDistinct,
    AgentInitiatedResultDelivered,
    CapacityLifecycleMatrix,
    BackendSetupDiscoveryMatrix,
    DynamicSessionDiscoveryMatrix,
    BackendNativeConfigDiscoveryMatrix,
}

impl CertificationCase {
    pub fn id(self) -> &'static str {
        match self {
            Self::CapabilityTruthMatrix => "capability_truth_matrix",
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
            Self::RequestUsageAggregatesToTurn => "request_usage_aggregates_to_turn",
            Self::RequestIdsUnique => "request_ids_unique",
            Self::RequestUsagePositive => "request_usage_positive",
            Self::RequestUsageLifecycleMatrix => "request_usage_lifecycle_matrix",
            Self::ContextUsagePresent => "context_usage_present",
            Self::ContextWindowValid => "context_window_valid",
            Self::ContextBreakdownPresent => "context_breakdown_present",
            Self::ReasoningDeltaReported => "reasoning_delta_reported",
            Self::TaskUpdateReported => "task_update_reported",
            Self::TaskUpdateLifecycleMatrix => "task_update_lifecycle_matrix",
            Self::TaskListReplacement => "task_list_replacement",
            Self::TaskListClear => "task_list_clear",
            Self::WorkflowProgressLifecycle => "workflow_progress_lifecycle",
            Self::OpaqueToolProgressReported => "opaque_tool_progress_reported",
            Self::RetryRecoveryMatrix => "retry_recovery_matrix",
            Self::RetryLifecycleMatrix => "retry_lifecycle_matrix",
            Self::RetryFaultInteractionMatrix => "retry_fault_interaction_matrix",
            Self::ConnectionCardinalityLifecycleMatrix => "connection_cardinality_lifecycle_matrix",
            Self::ProgressFamilyLifecycleMatrix => "progress_family_lifecycle_matrix",
            Self::ProgressConcurrencyMatrix => "progress_concurrency_matrix",
            Self::OrchestrationEventReported => "orchestration_event_reported",
            Self::OrchestrationPayloadMatrix => "orchestration_payload_matrix",
            Self::OrchestrationOutcomeMatrix => "orchestration_outcome_matrix",
            Self::OrchestrationCorrelationMatrix => "orchestration_correlation_matrix",
            Self::OrchestrationLifecycleMatrix => "orchestration_lifecycle_matrix",
            Self::CompactionObserved => "compaction_observed",
            Self::CompactionLifecycleMatrix => "compaction_lifecycle_matrix",
            Self::CompactionResumeExactlyOnce => "compaction_resume_exactly_once",
            Self::TeamContextCompactionContract => "team_context_compaction_contract",
            Self::CompactionCapabilityContract => "compaction_capability_contract",
            Self::ToolRequestEmitted => "tool_request_emitted",
            Self::ToolCompletionEmitted => "tool_completion_emitted",
            Self::ToolCallIdsCorrelate => "tool_call_ids_correlate",
            Self::ToolResponseOwnership => "tool_response_ownership",
            Self::ToolChangesWorkspace => "tool_changes_workspace",
            Self::ToolFailureReported => "tool_failure_reported",
            Self::ToolTurnReachesIdle => "tool_turn_reaches_idle",
            Self::GenericModifyFileContract => "generic_modify_file_contract",
            Self::GenericReadFilesContract => "generic_read_files_contract",
            Self::GenericSearchTypesContract => "generic_search_types_contract",
            Self::GenericGetTypeDocsContract => "generic_get_type_docs_contract",
            Self::GenericGenerateImageContract => "generic_generate_image_contract",
            Self::GenericWebSearchContract => "generic_web_search_contract",
            Self::GenericViewImageContract => "generic_view_image_contract",
            Self::GenericSleepContract => "generic_sleep_contract",
            Self::GenericOtherToolContract => "generic_other_tool_contract",
            Self::GenericToolLifecycleMatrix => "generic_tool_lifecycle_matrix",
            Self::InterruptEmitsCancellation => "interrupt_emits_cancellation",
            Self::InterruptReturnsIdle => "interrupt_returns_idle",
            Self::InterruptStopsCommand => "interrupt_stops_command",
            Self::FollowUpAfterInterrupt => "follow_up_after_interrupt",
            Self::InterruptFollowUpSurvivesRequestTimeout => {
                "interrupt_follow_up_survives_request_timeout"
            }
            Self::InterruptAllowsFollowUpDuringBackgroundWork => {
                "interrupt_allows_follow_up_during_background_work"
            }
            Self::InterruptPhaseContractMatrix => "interrupt_phase_contract_matrix",
            Self::InterruptForkContract => "interrupt_fork_contract",
            Self::ForegroundCommandContractMatrix => "foreground_command_contract_matrix",
            Self::BackgroundCommandContractMatrix => "background_command_contract_matrix",
            Self::BackgroundProgressLifecycle => "background_progress_lifecycle",
            Self::UserQuestionAnswerDuringBackgroundWork => {
                "user_question_answer_during_background_work"
            }
            Self::UserQuestionWaitsForAnswer => "user_question_waits_for_answer",
            Self::UserQuestionInterrupts => "user_question_interrupts",
            Self::UserQuestionCloses => "user_question_closes",
            Self::UserQuestionSurvivesReconnect => "user_question_survives_reconnect",
            Self::UserQuestionNotCopiedToFork => "user_question_not_copied_to_fork",
            Self::UserQuestionExpiresOnHostRestart => "user_question_expires_on_host_restart",
            Self::UserQuestionFailsOnTransportClose => "user_question_fails_on_transport_close",
            Self::UserQuestionResponseEdgeMatrix => "user_question_response_edge_matrix",
            Self::UserQuestionConcurrencyRaceMatrix => "user_question_concurrency_race_matrix",
            Self::PlanApprovalAnswerDuringBackgroundWork => {
                "plan_approval_answer_during_background_work"
            }
            Self::PlanApprovalWaitsForDecision => "plan_approval_waits_for_decision",
            Self::PlanApprovalInterrupts => "plan_approval_interrupts",
            Self::PlanApprovalCloses => "plan_approval_closes",
            Self::PlanApprovalSurvivesReconnect => "plan_approval_survives_reconnect",
            Self::PlanApprovalNotCopiedToFork => "plan_approval_not_copied_to_fork",
            Self::PlanApprovalExpiresOnHostRestart => "plan_approval_expires_on_host_restart",
            Self::PlanApprovalFailsOnTransportClose => "plan_approval_fails_on_transport_close",
            Self::PlanApprovalResponseEdgeMatrix => "plan_approval_response_edge_matrix",
            Self::PlanApprovalConcurrencyRaceMatrix => "plan_approval_concurrency_race_matrix",
            Self::MixedInteractionIsolationMatrix => "mixed_interaction_isolation_matrix",
            Self::AgentMessageAccepted => "agent_message_accepted",
            Self::AgentMessageChildFailureVisible => "agent_message_child_failure_visible",
            Self::AgentMessageContractMatrix => "agent_message_contract_matrix",
            Self::AgentAwaitStarts => "agent_await_starts",
            Self::AgentAwaitCompletesOnChildSuccess => "agent_await_completes_on_child_success",
            Self::AgentAwaitCompletesOnChildFailure => "agent_await_completes_on_child_failure",
            Self::AgentAwaitInterrupts => "agent_await_interrupts",
            Self::AgentAwaitCloses => "agent_await_closes",
            Self::AgentAwaitSurvivesReconnect => "agent_await_survives_reconnect",
            Self::AgentAwaitExpiresOnHostRestart => "agent_await_expires_on_host_restart",
            Self::AgentAwaitNotCopiedToFork => "agent_await_not_copied_to_fork",
            Self::AgentAwaitWaits => "agent_await_waits",
            Self::AgentAwaitFailsOnTransportClose => "agent_await_fails_on_transport_close",
            Self::AgentAwaitMultiChildContractMatrix => "agent_await_multi_child_contract_matrix",
            Self::AgentControlInvalidEdgeMatrix => "agent_control_invalid_edge_matrix",
            Self::AgentControlExhaustiveMatrix => "agent_control_exhaustive_matrix",
            Self::AgentControlSpawnContract => "agent_control_spawn_contract",
            Self::InputAdmissionQueueMatrix => "input_admission_queue_matrix",
            Self::NormalizedTurnShapeMatrix => "normalized_turn_shape_matrix",
            Self::PlainTerminationMatrix => "plain_termination_matrix",
            Self::UsageLifecycleMatrix => "usage_lifecycle_matrix",
            Self::UsageMetadataConcurrencyMatrix => "usage_metadata_concurrency_matrix",
            Self::SessionAppearsInList => "session_appears_in_list",
            Self::SessionListLifecycleMatrix => "session_list_lifecycle_matrix",
            Self::ResumeRemembersHistory => "resume_remembers_history",
            Self::ResumeAcceptsFollowUp => "resume_accepts_follow_up",
            Self::ResumePreservesWorkspace => "resume_preserves_workspace",
            Self::ToolMetadataSurvivesReconnectResume => "tool_metadata_survives_reconnect_resume",
            Self::ResumeOwnsStartupNotifications => "resume_owns_startup_notifications",
            Self::StartupLifecycleMatrix => "startup_lifecycle_matrix",
            Self::StartupForkLifecycleMatrix => "startup_fork_lifecycle_matrix",
            Self::AuthoritativeResumePreservesConcurrentEvents => {
                "authoritative_resume_preserves_concurrent_events"
            }
            Self::RepeatedResumeFollowUpExactlyOnce => "repeated_resume_follow_up_exactly_once",
            Self::PersistedQueueResumeNoFalseIdle => "persisted_queue_resume_no_false_idle",
            Self::ResumeClosedDispatchTerminalizesOnce => {
                "resume_closed_dispatch_terminalizes_once"
            }
            Self::ResumeBusyThenQuiescentDispatchesOnce => {
                "resume_busy_then_quiescent_dispatches_once"
            }
            Self::ForkCreatesDistinctSession => "fork_creates_distinct_session",
            Self::ForkPreservesHistory => "fork_preserves_history",
            Self::ForkAcceptsInitialPrompt => "fork_accepts_initial_prompt",
            Self::SessionSettingsApplied => "session_settings_applied",
            Self::MidTurnSteeringDelivered => "mid_turn_steering_delivered",
            Self::WorkspaceInstructionsObserved => "workspace_instructions_observed",
            Self::HostSteeringObserved => "host_steering_observed",
            Self::CustomizationSourcesCompose => "customization_sources_compose",
            Self::LiveCustomizationMatrix => "live_customization_matrix",
            Self::SkillLifecycleMatrix => "skill_lifecycle_matrix",
            Self::McpToolDiscovered => "mcp_tool_discovered",
            Self::McpToolCalled => "mcp_tool_called",
            Self::McpResultDelivered => "mcp_result_delivered",
            Self::McpEventsCorrelate => "mcp_events_correlate",
            Self::McpExactlyOnce => "mcp_exactly_once",
            Self::McpCompletionSuccessful => "mcp_completion_successful",
            Self::McpToolNamesCorrelate => "mcp_tool_names_correlate",
            Self::McpCanonicalResultPreserved => "mcp_canonical_result_preserved",
            Self::McpStartupAdversarialContract => "mcp_startup_adversarial_contract",
            Self::McpConnectionOwnershipMatrix => "mcp_connection_ownership_matrix",
            Self::McpPendingProcessDeathContract => "mcp_pending_process_death_contract",
            Self::McpReconnectContract => "mcp_reconnect_contract",
            Self::McpHostRestartResumeContract => "mcp_host_restart_resume_contract",
            Self::McpHttpTransportMatrix => "mcp_http_transport_matrix",
            Self::McpForkContract => "mcp_fork_contract",
            Self::ImageEchoPreserved => "image_echo_preserved",
            Self::ImageUnderstood => "image_understood",
            Self::ImageResponseStreams => "image_response_streams",
            Self::ImageMultipleAttachmentsPreserved => "image_multiple_attachments_preserved",
            Self::ImageFollowUpPreserved => "image_follow_up_preserved",
            Self::ImageAttachmentContractMatrix => "image_attachment_contract_matrix",
            Self::ImagePersistenceContractMatrix => "image_persistence_contract_matrix",
            Self::ReadOnlyAccessEnforced => "read_only_access_enforced",
            Self::NativeSubagentProjected => "native_subagent_projected",
            Self::NativeSubagentParentLinked => "native_subagent_parent_linked",
            Self::NativeSubagentUsageIsolated => "native_subagent_usage_isolated",
            Self::NativeSubagentResumeReplay => "native_subagent_resume_replay",
            Self::NativeSubagentForkReplay => "native_subagent_fork_replay",
            Self::AgentSpawnContractMatrix => "agent_spawn_contract_matrix",
            Self::BackgroundSubagentContractMatrix => "background_subagent_contract_matrix",
            Self::BackgroundWorkAllowsForegroundIdle => "background_work_allows_foreground_idle",
            Self::BackgroundWorkIsNotSupervisorStalled => {
                "background_work_is_not_supervisor_stalled"
            }
            Self::BackgroundCompletionReleasesAgent => "background_completion_releases_agent",
            Self::BackgroundCompletionRetiresTrayEntry => {
                "background_completion_retires_tray_entry"
            }
            Self::BackgroundCompletionResumesParent => "background_completion_resumes_parent",
            Self::AgentInitiatedTurnIsDistinct => "agent_initiated_turn_is_distinct",
            Self::AgentInitiatedResultDelivered => "agent_initiated_result_delivered",
            Self::CapacityLifecycleMatrix => "capacity_lifecycle_matrix",
            Self::BackendSetupDiscoveryMatrix => "backend_setup_discovery_matrix",
            Self::DynamicSessionDiscoveryMatrix => "dynamic_session_discovery_matrix",
            Self::BackendNativeConfigDiscoveryMatrix => "backend_native_config_discovery_matrix",
        }
    }

    pub fn tier(self) -> CertificationTier {
        match self {
            Self::NativeSubagentProjected
            | Self::NativeSubagentParentLinked
            | Self::NativeSubagentUsageIsolated
            | Self::NativeSubagentResumeReplay
            | Self::NativeSubagentForkReplay
            | Self::AgentSpawnContractMatrix
            | Self::BackgroundSubagentContractMatrix
            | Self::BackgroundWorkAllowsForegroundIdle
            | Self::BackgroundWorkIsNotSupervisorStalled
            | Self::BackgroundCompletionReleasesAgent
            | Self::BackgroundCompletionRetiresTrayEntry
            | Self::BackgroundCompletionResumesParent
            | Self::AgentInitiatedTurnIsDistinct
            | Self::AgentInitiatedResultDelivered => CertificationTier::SpecializedLive,
            Self::CapabilityTruthMatrix
            | Self::FollowUpCompletes
            | Self::FollowUpInputEchoedOnce
            | Self::FollowUpUsesDistinctMessage
            | Self::CumulativeUsageGrows
            | Self::InterruptEmitsCancellation
            | Self::InterruptReturnsIdle
            | Self::InterruptStopsCommand
            | Self::FollowUpAfterInterrupt
            | Self::InterruptFollowUpSurvivesRequestTimeout
            | Self::InterruptAllowsFollowUpDuringBackgroundWork
            | Self::InterruptPhaseContractMatrix
            | Self::InterruptForkContract
            | Self::ForegroundCommandContractMatrix
            | Self::BackgroundCommandContractMatrix
            | Self::BackgroundProgressLifecycle
            | Self::UserQuestionAnswerDuringBackgroundWork
            | Self::UserQuestionWaitsForAnswer
            | Self::UserQuestionInterrupts
            | Self::UserQuestionCloses
            | Self::UserQuestionSurvivesReconnect
            | Self::UserQuestionNotCopiedToFork
            | Self::UserQuestionExpiresOnHostRestart
            | Self::UserQuestionFailsOnTransportClose
            | Self::UserQuestionResponseEdgeMatrix
            | Self::UserQuestionConcurrencyRaceMatrix
            | Self::PlanApprovalAnswerDuringBackgroundWork
            | Self::PlanApprovalWaitsForDecision
            | Self::PlanApprovalInterrupts
            | Self::PlanApprovalCloses
            | Self::PlanApprovalSurvivesReconnect
            | Self::PlanApprovalNotCopiedToFork
            | Self::PlanApprovalExpiresOnHostRestart
            | Self::PlanApprovalFailsOnTransportClose
            | Self::PlanApprovalResponseEdgeMatrix
            | Self::PlanApprovalConcurrencyRaceMatrix
            | Self::MixedInteractionIsolationMatrix
            | Self::AgentMessageAccepted
            | Self::AgentMessageChildFailureVisible
            | Self::AgentMessageContractMatrix
            | Self::AgentAwaitStarts
            | Self::AgentAwaitCompletesOnChildSuccess
            | Self::AgentAwaitCompletesOnChildFailure
            | Self::AgentAwaitInterrupts
            | Self::AgentAwaitCloses
            | Self::AgentAwaitSurvivesReconnect
            | Self::AgentAwaitExpiresOnHostRestart
            | Self::AgentAwaitNotCopiedToFork
            | Self::AgentAwaitWaits
            | Self::AgentAwaitFailsOnTransportClose
            | Self::AgentAwaitMultiChildContractMatrix
            | Self::AgentControlInvalidEdgeMatrix
            | Self::AgentControlExhaustiveMatrix
            | Self::AgentControlSpawnContract
            | Self::InputAdmissionQueueMatrix
            | Self::NormalizedTurnShapeMatrix
            | Self::PlainTerminationMatrix
            | Self::UsageLifecycleMatrix
            | Self::UsageMetadataConcurrencyMatrix
            | Self::RequestUsageLifecycleMatrix
            | Self::SessionAppearsInList
            | Self::SessionListLifecycleMatrix
            | Self::ResumeRemembersHistory
            | Self::ResumeAcceptsFollowUp
            | Self::ResumePreservesWorkspace
            | Self::ToolMetadataSurvivesReconnectResume
            | Self::ResumeOwnsStartupNotifications
            | Self::StartupLifecycleMatrix
            | Self::StartupForkLifecycleMatrix
            | Self::AuthoritativeResumePreservesConcurrentEvents
            | Self::RepeatedResumeFollowUpExactlyOnce
            | Self::PersistedQueueResumeNoFalseIdle
            | Self::ResumeClosedDispatchTerminalizesOnce
            | Self::ResumeBusyThenQuiescentDispatchesOnce
            | Self::ForkCreatesDistinctSession
            | Self::ForkPreservesHistory
            | Self::ForkAcceptsInitialPrompt
            | Self::SessionSettingsApplied
            | Self::MidTurnSteeringDelivered
            | Self::TaskUpdateReported
            | Self::TaskUpdateLifecycleMatrix
            | Self::TaskListReplacement
            | Self::TaskListClear
            | Self::WorkflowProgressLifecycle
            | Self::OpaqueToolProgressReported
            | Self::RetryRecoveryMatrix
            | Self::RetryLifecycleMatrix
            | Self::RetryFaultInteractionMatrix
            | Self::ConnectionCardinalityLifecycleMatrix
            | Self::ProgressFamilyLifecycleMatrix
            | Self::ProgressConcurrencyMatrix
            | Self::OrchestrationPayloadMatrix
            | Self::OrchestrationOutcomeMatrix
            | Self::OrchestrationCorrelationMatrix
            | Self::OrchestrationLifecycleMatrix
            | Self::McpStartupAdversarialContract
            | Self::McpConnectionOwnershipMatrix
            | Self::McpPendingProcessDeathContract
            | Self::McpReconnectContract
            | Self::McpHostRestartResumeContract
            | Self::McpHttpTransportMatrix
            | Self::McpForkContract
            | Self::ImageAttachmentContractMatrix
            | Self::ImagePersistenceContractMatrix
            | Self::CompactionObserved
            | Self::CompactionLifecycleMatrix
            | Self::CompactionResumeExactlyOnce
            | Self::TeamContextCompactionContract
            | Self::CompactionCapabilityContract
            | Self::GenericToolLifecycleMatrix
            | Self::LiveCustomizationMatrix
            | Self::SkillLifecycleMatrix
            | Self::CapacityLifecycleMatrix
            | Self::BackendSetupDiscoveryMatrix
            | Self::DynamicSessionDiscoveryMatrix
            | Self::BackendNativeConfigDiscoveryMatrix => CertificationTier::StatefulLive,
            _ => CertificationTier::CheapLive,
        }
    }

    pub fn required_capabilities(self) -> &'static [BackendCapability] {
        use BackendCapability as Capability;

        match self {
            Self::CapabilityTruthMatrix => &[],
            Self::TurnUsagePresent
            | Self::TurnInputTokensPositive
            | Self::TurnOutputTokensPositive
            | Self::TurnTotalConsistent
            | Self::CumulativeUsageGrows => &[Capability::TurnUsageReported],
            Self::RequestUsagePresent
            | Self::RequestSequenceStartsAtOne
            | Self::RequestSequencesContiguous
            | Self::RequestUsageMatchesTurn
            | Self::RequestIdsUnique
            | Self::RequestUsagePositive
            | Self::RequestUsageLifecycleMatrix => &[Capability::ModelRequestUsageReported],
            Self::RequestUsageAggregatesToTurn => &[
                Capability::ModelRequestUsageReported,
                Capability::TurnUsageReported,
            ],
            Self::ContextUsagePresent | Self::ContextWindowValid => {
                &[Capability::ContextUsageReported]
            }
            Self::ContextBreakdownPresent => &[Capability::ContextBreakdownReported],
            Self::ReasoningDeltaReported => &[Capability::ReasoningDeltas],
            Self::TaskUpdateReported | Self::TaskUpdateLifecycleMatrix => {
                &[Capability::TaskUpdates]
            }
            Self::TaskListReplacement => {
                &[Capability::TaskUpdates, Capability::TaskListReplacement]
            }
            Self::TaskListClear => &[Capability::TaskUpdates, Capability::TaskListClear],
            Self::WorkflowProgressLifecycle => &[Capability::WorkflowProgress],
            Self::OpaqueToolProgressReported => &[Capability::OpaqueToolProgress],
            Self::RetryRecoveryMatrix => &[],
            Self::RetryLifecycleMatrix => &[Capability::RetryTelemetry],
            Self::RetryFaultInteractionMatrix => &[Capability::RetryTelemetry],
            Self::ConnectionCardinalityLifecycleMatrix => &[],
            Self::OrchestrationEventReported
            | Self::OrchestrationPayloadMatrix
            | Self::OrchestrationOutcomeMatrix
            | Self::OrchestrationCorrelationMatrix
            | Self::OrchestrationLifecycleMatrix => &[Capability::OrchestrationEvents],
            Self::CompactionObserved
            | Self::CompactionLifecycleMatrix
            | Self::TeamContextCompactionContract => &[Capability::CompactionReported],
            Self::CompactionResumeExactlyOnce => {
                &[Capability::CompactionReported, Capability::ResumeSession]
            }
            Self::InterruptEmitsCancellation
            | Self::InterruptReturnsIdle
            | Self::InterruptStopsCommand
            | Self::FollowUpAfterInterrupt
            | Self::InterruptFollowUpSurvivesRequestTimeout
            | Self::InterruptPhaseContractMatrix => &[Capability::Interrupt],
            Self::InterruptAllowsFollowUpDuringBackgroundWork => {
                &[Capability::Interrupt, Capability::BackgroundTasks]
            }
            Self::InterruptForkContract => &[Capability::Interrupt, Capability::ForkSession],
            Self::UserQuestionAnswerDuringBackgroundWork => &[
                Capability::UserQuestionRequests,
                Capability::BackgroundTasks,
            ],
            Self::UserQuestionInterrupts => {
                &[Capability::UserQuestionRequests, Capability::Interrupt]
            }
            Self::UserQuestionWaitsForAnswer
            | Self::UserQuestionCloses
            | Self::UserQuestionSurvivesReconnect
            | Self::UserQuestionFailsOnTransportClose => &[Capability::UserQuestionRequests],
            Self::UserQuestionResponseEdgeMatrix | Self::UserQuestionConcurrencyRaceMatrix => {
                &[Capability::UserQuestionRequests]
            }
            Self::UserQuestionNotCopiedToFork => {
                &[Capability::UserQuestionRequests, Capability::ForkSession]
            }
            Self::UserQuestionExpiresOnHostRestart => {
                &[Capability::UserQuestionRequests, Capability::ResumeSession]
            }
            Self::PlanApprovalAnswerDuringBackgroundWork => &[
                Capability::PlanApprovalRequests,
                Capability::BackgroundTasks,
            ],
            Self::PlanApprovalInterrupts => {
                &[Capability::PlanApprovalRequests, Capability::Interrupt]
            }
            Self::PlanApprovalWaitsForDecision
            | Self::PlanApprovalCloses
            | Self::PlanApprovalSurvivesReconnect
            | Self::PlanApprovalFailsOnTransportClose => &[Capability::PlanApprovalRequests],
            Self::PlanApprovalResponseEdgeMatrix | Self::PlanApprovalConcurrencyRaceMatrix => {
                &[Capability::PlanApprovalRequests]
            }
            Self::MixedInteractionIsolationMatrix => &[
                Capability::UserQuestionRequests,
                Capability::PlanApprovalRequests,
            ],
            Self::PlanApprovalNotCopiedToFork => {
                &[Capability::PlanApprovalRequests, Capability::ForkSession]
            }
            Self::PlanApprovalExpiresOnHostRestart => {
                &[Capability::PlanApprovalRequests, Capability::ResumeSession]
            }
            Self::AgentMessageAccepted
            | Self::AgentMessageChildFailureVisible
            | Self::AgentMessageContractMatrix
            | Self::AgentAwaitStarts
            | Self::AgentAwaitCompletesOnChildSuccess
            | Self::AgentAwaitCompletesOnChildFailure
            | Self::AgentAwaitCloses
            | Self::AgentAwaitSurvivesReconnect
            | Self::AgentAwaitWaits
            | Self::AgentAwaitFailsOnTransportClose
            | Self::AgentAwaitMultiChildContractMatrix
            | Self::AgentControlInvalidEdgeMatrix => &[Capability::AgentControlTools],
            Self::AgentControlExhaustiveMatrix | Self::AgentControlSpawnContract => {
                &[Capability::AgentControlTools]
            }
            Self::InputAdmissionQueueMatrix => &[],
            Self::NormalizedTurnShapeMatrix | Self::PlainTerminationMatrix => &[],
            Self::ProgressFamilyLifecycleMatrix => &[],
            Self::ProgressConcurrencyMatrix => &[Capability::BackgroundTasks],
            Self::UsageLifecycleMatrix => &[Capability::TurnUsageReported],
            Self::UsageMetadataConcurrencyMatrix => &[Capability::TurnUsageReported],
            Self::AgentAwaitInterrupts => &[Capability::AgentControlTools, Capability::Interrupt],
            Self::AgentAwaitExpiresOnHostRestart => {
                &[Capability::AgentControlTools, Capability::ResumeSession]
            }
            Self::AgentAwaitNotCopiedToFork => {
                &[Capability::AgentControlTools, Capability::ForkSession]
            }
            Self::SessionAppearsInList => &[Capability::ListSessions],
            Self::SessionListLifecycleMatrix => &[],
            Self::ResumeRemembersHistory
            | Self::ResumeAcceptsFollowUp
            | Self::ResumePreservesWorkspace
            | Self::ResumeOwnsStartupNotifications
            | Self::StartupLifecycleMatrix
            | Self::RepeatedResumeFollowUpExactlyOnce
            | Self::PersistedQueueResumeNoFalseIdle
            | Self::ResumeClosedDispatchTerminalizesOnce
            | Self::ToolMetadataSurvivesReconnectResume => &[Capability::ResumeSession],
            Self::StartupForkLifecycleMatrix => {
                &[Capability::ResumeSession, Capability::ForkSession]
            }
            Self::ResumeBusyThenQuiescentDispatchesOnce => &[
                Capability::ResumeSession,
                Capability::BackgroundTasks,
                Capability::AgentInitiatedTurns,
            ],
            Self::AuthoritativeResumePreservesConcurrentEvents => {
                &[Capability::ResumeSession, Capability::BackgroundTasks]
            }
            Self::ForkCreatesDistinctSession
            | Self::ForkPreservesHistory
            | Self::ForkAcceptsInitialPrompt => &[Capability::ForkSession],
            Self::SessionSettingsApplied => &[Capability::SessionSettings],
            Self::MidTurnSteeringDelivered => &[Capability::MidTurnSteering],
            Self::WorkspaceInstructionsObserved => &[Capability::WorkspaceInstructions],
            Self::HostSteeringObserved | Self::SkillLifecycleMatrix => &[Capability::Customization],
            Self::CustomizationSourcesCompose => {
                &[Capability::Customization, Capability::WorkspaceInstructions]
            }
            Self::LiveCustomizationMatrix => &[Capability::Customization],
            Self::McpToolDiscovered
            | Self::McpToolCalled
            | Self::McpResultDelivered
            | Self::McpEventsCorrelate
            | Self::McpExactlyOnce
            | Self::McpCompletionSuccessful
            | Self::McpToolNamesCorrelate
            | Self::McpCanonicalResultPreserved
            | Self::McpStartupAdversarialContract
            | Self::McpConnectionOwnershipMatrix
            | Self::McpPendingProcessDeathContract
            | Self::McpReconnectContract => &[Capability::StartupMcpServers],
            Self::McpHttpTransportMatrix => &[Capability::StartupMcpServers],
            Self::McpForkContract => &[Capability::StartupMcpServers, Capability::ForkSession],
            Self::McpHostRestartResumeContract => {
                &[Capability::StartupMcpServers, Capability::ResumeSession]
            }
            Self::GenericModifyFileContract => &[Capability::GenericModifyFile],
            Self::GenericReadFilesContract => &[Capability::GenericReadFiles],
            Self::GenericSearchTypesContract => &[Capability::GenericSearchTypes],
            Self::GenericGetTypeDocsContract => &[Capability::GenericGetTypeDocs],
            Self::GenericGenerateImageContract => &[Capability::GenericGenerateImage],
            Self::GenericWebSearchContract => &[Capability::GenericWebSearch],
            Self::GenericViewImageContract => &[Capability::GenericViewImage],
            Self::GenericSleepContract => &[Capability::GenericSleep],
            Self::GenericOtherToolContract => {
                &[Capability::GenericOtherTool, Capability::StartupMcpServers]
            }
            Self::GenericToolLifecycleMatrix => &[],
            Self::ImageEchoPreserved
            | Self::ImageUnderstood
            | Self::ImageResponseStreams
            | Self::ImageMultipleAttachmentsPreserved
            | Self::ImageFollowUpPreserved
            | Self::ImageAttachmentContractMatrix => &[Capability::ImageInput],
            Self::ImagePersistenceContractMatrix => &[Capability::ImageInput],
            Self::NativeSubagentProjected | Self::NativeSubagentParentLinked => {
                &[Capability::Subagents]
            }
            Self::NativeSubagentUsageIsolated => {
                &[Capability::Subagents, Capability::TurnUsageReported]
            }
            Self::NativeSubagentResumeReplay => &[Capability::Subagents, Capability::ResumeSession],
            Self::NativeSubagentForkReplay => &[Capability::Subagents, Capability::ForkSession],
            Self::AgentSpawnContractMatrix => &[Capability::ForegroundSubagents],
            Self::BackgroundSubagentContractMatrix => {
                &[Capability::BackgroundSubagents, Capability::BackgroundTasks]
            }
            Self::BackgroundWorkAllowsForegroundIdle
            | Self::BackgroundWorkIsNotSupervisorStalled
            | Self::BackgroundCommandContractMatrix
            | Self::BackgroundProgressLifecycle
            | Self::BackgroundCompletionReleasesAgent
            | Self::BackgroundCompletionRetiresTrayEntry => &[Capability::BackgroundTasks],
            Self::BackgroundCompletionResumesParent
            | Self::AgentInitiatedTurnIsDistinct
            | Self::AgentInitiatedResultDelivered => {
                &[Capability::AgentInitiatedTurns, Capability::BackgroundTasks]
            }
            Self::CapacityLifecycleMatrix => &[Capability::CapacityTelemetry],
            _ => &[],
        }
    }
}
