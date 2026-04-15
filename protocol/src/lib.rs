#[cfg(feature = "framing")]
pub mod framing;
pub mod types;
pub mod validator;

#[cfg(feature = "framing")]
pub use framing::{FrameError, read_envelope, write_envelope};
pub use types::{
    AgentErrorCode, AgentErrorPayload, AgentId, AgentInput, AgentStartPayload, BackendKind,
    BootstrapData, ChatEvent, ChatMessage, ContextBreakdown, DumpSettingsPayload, Envelope,
    FileEntryOp, FileInfo, FrameKind, HelloPayload, HostSettingValue, HostSettings,
    HostSettingsPayload, ImageData, InterruptPayload, ListSessionsPayload, MessageSender,
    ModelInfo, NewAgentPayload, NewTerminalPayload, OperationCancelledData, PROTOCOL_VERSION,
    Project, ProjectAddRootPayload, ProjectCreatePayload, ProjectDeletePayload, ProjectDiffScope,
    ProjectFileContentsPayload, ProjectFileEntry, ProjectFileKind, ProjectFileListPayload,
    ProjectGitChangeKind, ProjectGitDiffFile, ProjectGitDiffHunk, ProjectGitDiffLine,
    ProjectGitDiffLineKind, ProjectGitDiffPayload, ProjectGitFileStatus, ProjectGitStatusPayload,
    ProjectId, ProjectListDirPayload, ProjectNotifyPayload, ProjectPath, ProjectReadDiffPayload,
    ProjectReadFilePayload, ProjectRefreshPayload, ProjectRenamePayload, ProjectRootGitStatus,
    ProjectRootListing, ProjectRootPath, ProjectStageFilePayload, ProjectStageHunkPayload,
    ReasoningData, RejectCode, RejectPayload, RetryAttemptData, SendMessagePayload, SeqValidator,
    SessionId, SessionListPayload, SessionSummary, SetSettingPayload, SpawnAgentParams,
    SpawnAgentPayload, SpawnCostHint, StreamEndData, StreamPath, StreamStartData,
    StreamTextDeltaData, TYDE_VERSION, Task, TaskList, TaskStatus, TerminalClosePayload,
    TerminalCreatePayload, TerminalErrorCode, TerminalErrorPayload, TerminalExitPayload,
    TerminalId, TerminalLaunchTarget, TerminalOutputPayload, TerminalResizePayload,
    TerminalSendPayload, TerminalStartPayload, TokenUsage, ToolExecutionCompletedData,
    ToolExecutionResult, ToolRequest, ToolRequestType, ToolUseData, Version, WelcomePayload,
};
pub use validator::{ObservedFrame, ProtocolValidator, ProtocolViolation};
