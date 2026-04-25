#[cfg(feature = "framing")]
pub mod framing;
pub mod types;
pub mod validator;

#[cfg(feature = "framing")]
pub use framing::{FrameError, read_envelope, write_envelope};
pub use types::{
    AgentClosedPayload, AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, AgentOrigin,
    AgentRenamedPayload, AgentStartPayload, BackendKind, BackendSetupAction, BackendSetupCommand,
    BackendSetupInfo, BackendSetupPayload, BackendSetupStatus, BootstrapData,
    CancelQueuedMessagePayload, ChatEvent, ChatMessage, CloseAgentPayload, CommandErrorCode,
    CommandErrorPayload, ContextBreakdown, CustomAgent, CustomAgentDeletePayload, CustomAgentId,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, DeleteSessionPayload, DiffContextMode,
    EditQueuedMessagePayload, Envelope, FileEntryOp, FileInfo, FrameKind, HelloPayload,
    HostAbsPath, HostBrowseClosePayload, HostBrowseEntriesPayload, HostBrowseEntry,
    HostBrowseEntryError, HostBrowseErrorCode, HostBrowseErrorPayload, HostBrowseListPayload,
    HostBrowseOpenedPayload, HostBrowseStartPayload, HostPlatform, HostSettingValue, HostSettings,
    HostSettingsPayload, ImageData, InterruptPayload, ListSessionsPayload, McpServerConfig,
    McpServerDeletePayload, McpServerId, McpServerNotifyPayload, McpServerUpsertPayload,
    McpTransportConfig, MessageSender, ModelInfo, NewAgentPayload, NewTerminalPayload,
    OperationCancelledData, PROTOCOL_VERSION, Project, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeletePayload, ProjectDiffScope, ProjectDiscardFilePayload, ProjectFileContentsPayload,
    ProjectFileEntry, ProjectFileKind, ProjectFileListPayload, ProjectGitChangeKind,
    ProjectGitCommitPayload, ProjectGitCommitResultPayload, ProjectGitDiffFile, ProjectGitDiffHunk,
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload, ProjectGitFileStatus,
    ProjectGitStatusPayload, ProjectId, ProjectListDirPayload, ProjectNotifyPayload, ProjectPath,
    ProjectReadDiffPayload, ProjectReadFilePayload, ProjectRefreshPayload, ProjectRenamePayload,
    ProjectReorderPayload, ProjectRootGitStatus, ProjectRootListing, ProjectRootPath,
    ProjectStageFilePayload, ProjectStageHunkPayload, ProjectUnstageFilePayload,
    QueuedMessageEntry, QueuedMessageId, QueuedMessagesPayload, ReasoningData, RejectCode,
    RejectPayload, RetryAttemptData, RunBackendSetupPayload, SelectOption, SendMessagePayload,
    SendQueuedMessageNowPayload, SeqValidator, SessionId, SessionListPayload, SessionSchemaEntry,
    SessionSchemasPayload, SessionSettingField, SessionSettingFieldType, SessionSettingValue,
    SessionSettingsPayload, SessionSettingsSchema, SessionSettingsValues, SessionSummary,
    SetAgentNamePayload, SetSessionSettingsPayload, SetSettingPayload, Skill, SkillId,
    SkillNotifyPayload, SkillRefreshPayload, SpawnAgentParams, SpawnAgentPayload, SpawnCostHint,
    Steering, SteeringDeletePayload, SteeringId, SteeringNotifyPayload, SteeringScope,
    SteeringUpsertPayload, StreamEndData, StreamPath, StreamStartData, StreamTextDeltaData,
    TYDE_VERSION, Task, TaskList, TaskStatus, TerminalClosePayload, TerminalCreatePayload,
    TerminalErrorCode, TerminalErrorPayload, TerminalExitPayload, TerminalId, TerminalLaunchTarget,
    TerminalOutputPayload, TerminalResizePayload, TerminalSendPayload, TerminalStartPayload,
    TokenUsage, ToolExecutionCompletedData, ToolExecutionResult, ToolPolicy, ToolRequest,
    ToolRequestType, ToolUseData, Version, WelcomePayload,
};
pub use validator::{ObservedFrame, ProtocolValidator, ProtocolViolation};
