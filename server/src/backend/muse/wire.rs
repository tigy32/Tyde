use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Session {
    pub session_id: String,
    pub path: String,
    pub workspace_root: Option<String>,
    pub model_id: Option<String>,
    pub status: SessionStatus,
    pub active_turn_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub permission_state: Option<PermissionState>,
    pub kind: Option<SessionKind>,
    pub parent_session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum SessionKind {
    Root,
    Subagent,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct SubagentControl {
    pub record: SubagentRecord,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum SubagentRecord {
    SpawnAccepted {
        subagent_id: String,
        parent_run_id: String,
        objective: String,
        role: String,
    },
    StartAttested {
        subagent_id: String,
        subagent_session_id: String,
        dispatched_via_tool_call_id: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum SessionStatus {
    NotLoaded,
    Idle,
    Running,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PermissionState {
    pub profile_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionResult {
    pub session: Session,
    pub history: Option<History>,
    pub view_cursor: String,
    pub last_turn: Option<LastTurn>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LastTurn {
    pub turn_id: String,
    pub terminal: Terminal,
}

#[derive(Deserialize)]
pub(super) struct History {
    pub snapshot: Option<Snapshot>,
}

#[derive(Deserialize)]
pub(super) struct Snapshot {
    pub state: SnapshotState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotState {
    pub goal: Option<Goal>,
    pub reasoning_effort: Option<EffortState>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EffortState {
    pub reasoning_effort: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Model {
    pub model_id: String,
    pub display_label: String,
    pub is_default: bool,
    pub is_active: bool,
    pub context_limit: Option<u64>,
    pub default_reasoning_effort: Option<String>,
    pub variants: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct Models {
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Item {
    pub item_id: String,
    pub kind: ItemKind,
    pub status: ItemStatus,
    pub revision: u64,
    pub call_id: Option<String>,
    pub command_id: Option<String>,
    pub attachments: Option<Vec<MessageAttachment>>,
    pub tool: Option<String>,
    pub text: Option<String>,
    pub display_text: Option<String>,
    pub visible_output: Option<String>,
    pub failure_reason: Option<String>,
    pub background: Option<bool>,
    pub patch_ref: Option<OutputRef>,
    pub patch_summary: Option<PatchSummary>,
    pub status_line: Option<String>,
    pub output_ref: Option<OutputRef>,
    pub truncated: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MessageAttachment {
    #[serde(rename = "type")]
    pub kind: String,
    pub media_type: String,
}

#[derive(Deserialize)]
pub(super) struct CommandIntake {
    pub record: ReceivedCommand,
}

#[derive(Deserialize)]
pub(super) struct ReceivedCommand {
    pub command_id: String,
    pub session_stream: StreamRef,
    pub command: IntakeCommand,
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
pub(super) enum IntakeCommand {
    #[serde(rename = "turn_queue_submit")]
    TurnQueueSubmit { payload: QueuedInput },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct QueuedInput {
    pub attachments: Option<Vec<InputAttachment>>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(super) enum InputAttachment {
    Image {
        base64_data: String,
        media_type: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskCommandResult {
    pub command_id: String,
    pub task_id: String,
    pub status: TaskCommandStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum TaskCommandStatus {
    Accepted,
}

#[derive(Deserialize)]
pub(super) struct TaskInventory {
    pub tasks: Vec<BackgroundTask>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BackgroundTask {
    pub task_id: String,
    pub status: ItemStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BashExecutionState {
    BackgroundRunning,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolCall,
    Compaction,
    Subagent,
    Workflow,
    ReminderChild,
    HookRun,
    SideChat,
    UserShell,
    WebSearch,
    SkillRead,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Rejected,
    TimedOut,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OutputRef {
    pub id: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct PatchSummary {
    pub added: u64,
    pub removed: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ItemEvent {
    pub session_id: String,
    pub item: Item,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Delta {
    pub session_id: String,
    pub item_id: String,
    pub field: Option<String>,
    pub delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnEvent {
    pub session_id: String,
    pub turn_id: String,
    pub terminal: Option<Terminal>,
    pub error: Option<TurnError>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum Terminal {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Deserialize)]
pub(super) struct TurnError {
    pub kind: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct RawRecord {
    pub id: String,
    pub sequence: u64,
    pub stream: StreamRef,
    pub schema_version: u32,
    pub payload_schema_version: u32,
    pub payload_type: String,
    pub payload: Value,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum RuntimeEvent {
    Run {
        run_id: String,
        event: RunEvent,
    },
    Task {
        task_id: String,
        event: TaskEvent,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum TaskEvent {
    Proposed {
        task_kind: String,
    },
    Output {
        chunk: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum RunEvent {
    Started,
    Terminal {
        terminal: Terminal,
    },
    ContextCompactionInstalled {
        candidate_id: String,
        install_id: String,
        budget_before: CompactionBudget,
        budget_after: CompactionBudget,
    },
    ModelRequestConfigured {
        toolset: Toolset,
    },
    ModelResponseCreated {
        response_id: String,
    },
    ToolResultBatchCommitted {
        results: Vec<CommittedToolResult>,
    },
    ToolResultModelVisibleContent {
        call_id: String,
        content: Vec<ModelVisibleContent>,
    },
    AssistantMessageCommitted {
        message_id: String,
        response_id: String,
        text: String,
    },
    AssistantToolCallsCommitted {
        response_id: String,
        tool_calls: Vec<ToolCall>,
    },
    ReasoningSummaryCommitted {
        message_id: String,
        response_id: String,
        text: String,
    },
    ReasoningCommitted {
        response_id: String,
        text: String,
    },
    ModelCompleted {
        model: String,
        finish_reason: Option<String>,
        usage: Usage,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
pub(super) struct CompactionBudget {
    pub estimated_prompt_tokens: u64,
}

#[derive(Deserialize)]
pub(super) struct ManualCompactionTarget {
    pub command_id: String,
    pub run_stream: StreamRef,
}

#[derive(Deserialize)]
pub(super) struct ManualCompactionOutcome {
    pub command_id: String,
    pub run_stream: StreamRef,
    pub candidate_id: Option<String>,
    pub terminal_outcome: CompactionOutcome,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CompactionOutcome {
    Compacted,
    Noop,
    Failed,
    Cancelled,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompactAccepted {
    pub command_id: String,
    pub status: CompactStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum CompactStatus {
    Accepted,
    Noop,
}

#[derive(Clone, Deserialize)]
pub(super) struct CommittedToolResult {
    pub tool_call_id: String,
    pub text: String,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ModelVisibleContent {
    Image {
        base64_data: Option<String>,
        media_type: String,
    },
}

#[derive(Deserialize)]
pub(super) struct GoalControlApplied {
    pub record: AppliedGoal,
}

#[derive(Deserialize)]
pub(super) struct AppliedGoal {
    pub goal: Option<Goal>,
}

#[derive(Clone, Deserialize)]
pub(super) struct Toolset {
    pub active_tools: Vec<String>,
    #[serde(default)]
    pub mode: ToolsetMode,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolsetMode {
    All,
    Named,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
pub(super) struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args: String,
}

#[derive(Clone, Default, Deserialize)]
pub(super) struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct StreamRef {
    pub id: String,
    pub kind: String,
}

#[derive(Clone, serde::Serialize, Deserialize)]
pub(super) struct Position {
    pub id: String,
    pub sequence: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ViewPage {
    pub events: Vec<ViewEvent>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ViewEvent {
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Deserialize)]
pub(super) struct Goal {
    pub objective: String,
    pub status: String,
}

#[derive(Deserialize)]
pub(super) struct GoalEvent {
    pub goal: Option<Goal>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeQuestion {
    pub user_input_id: String,
    pub tool_call_id: String,
    pub questions: Vec<Question>,
}

#[derive(Clone, Deserialize)]
pub(super) struct Question {
    pub id: String,
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub selection: Selection,
}

#[derive(Clone, Deserialize)]
pub(super) struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Selection {
    pub mode: SelectionMode,
    pub min_selections: Option<u32>,
    pub max_selections: Option<u32>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum SelectionMode {
    Single,
    Multiple,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) enum QuestionOutcome {
    Answered,
    Cancelled,
    Interrupted,
    Clarified,
    TimedOut,
    Aborted,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuestionSettled {
    pub user_input_id: String,
    pub outcome: QuestionOutcome,
    pub answers: Vec<NativeQuestionAnswer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeQuestionAnswer {
    pub question_id: String,
    pub selected_label: Option<String>,
    pub selected_labels: Option<Vec<String>>,
    pub free_text: Option<String>,
    pub note: Option<String>,
}

impl NativeQuestionAnswer {
    pub fn to_protocol(&self) -> Result<protocol::UserQuestionAnswer, String> {
        let answer = match (&self.selected_label, &self.selected_labels, &self.free_text) {
            (Some(label), None, None) => protocol::UserQuestionAnswerValue::Selected {
                labels: vec![label.clone()],
                note: self.note.clone(),
            },
            (None, Some(labels), None) => protocol::UserQuestionAnswerValue::Selected {
                labels: labels.clone(),
                note: self.note.clone(),
            },
            (None, None, Some(text)) => protocol::UserQuestionAnswerValue::FreeText {
                text: text.clone(),
                note: self.note.clone(),
            },
            _ => return Err("Muse settled a question without exactly one answer form".to_owned()),
        };
        Ok(protocol::UserQuestionAnswer {
            question_id: protocol::UserQuestionId(self.question_id.clone()),
            answer,
        })
    }
}

#[derive(Clone, Deserialize)]
pub(super) struct CommandResult {
    pub exit_code: i32,
    pub output: String,
}
