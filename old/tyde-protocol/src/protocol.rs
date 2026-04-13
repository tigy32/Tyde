//! Canonical Tyde protocol specification.
//!
//! This file is the single Rust source of truth for the shared wire protocol.
//! TypeScript bindings in `packages/protocol/src/generated/` are generated from
//! the types defined here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::VariantNames;
use ts_rs::{Config, TS};

pub const PROTOCOL_VERSION: u32 = 2;

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn deserialize_u64_from_number_or_string<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct U64Visitor;

    impl<'de> serde::de::Visitor<'de> for U64Visitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("u64 as number or string")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(v)
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v < 0 {
                return Err(E::custom("negative values are not valid u64"));
            }
            Ok(v as u64)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            v.parse::<u64>()
                .map_err(|_| E::custom(format!("invalid u64 string: {v}")))
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(U64Visitor)
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "type")]
pub enum ClientFrame {
    Invoke {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        command: String,
        params: Value,
    },
    Handshake {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        protocol_version: u32,
        #[serde(default)]
        tyde_version: String,
        last_agent_event_seq: u64,
        last_chat_event_seqs: HashMap<String, u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "type")]
pub enum ServerFrame {
    Result {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        data: Value,
    },
    Error {
        #[serde(deserialize_with = "deserialize_u64_from_number_or_string")]
        req_id: u64,
        error: String,
    },
    Event {
        event: String,
        seq: Option<u64>,
        payload: Value,
    },
    Shutdown {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(bound(
    serialize = "Agent: Serialize, SessionRecord: Serialize, ProjectRecord: Serialize",
    deserialize = "Agent: Deserialize<'de>, SessionRecord: Deserialize<'de>, ProjectRecord: Deserialize<'de>"
))]
pub struct HandshakeResult<Agent = Value, SessionRecord = Value, ProjectRecord = Value> {
    pub protocol_version: u32,
    #[serde(default)]
    pub tyde_version: String,
    pub agents: Vec<Agent>,
    pub conversations: Vec<ConversationSnapshot>,
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub session_records: Vec<SessionRecord>,
    #[serde(default)]
    pub projects: Vec<ProjectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConversationSnapshot {
    pub agent_id: String,
    pub backend_kind: String,
    pub workspace_roots: Vec<String>,
    pub chat_event_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ModuleSchemaInfo {
    pub namespace: String,
    pub schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConversationRegisteredData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub agent_id: Option<String>,
    pub workspace_roots: Vec<String>,
    pub backend_kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub ui_owner_project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct DurationLike {
    pub secs: u64,
    pub nanos: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ContextBreakdown {
    pub system_prompt_bytes: u64,
    pub tool_io_bytes: u64,
    pub conversation_history_bytes: u64,
    pub reasoning_bytes: u64,
    pub context_injection_bytes: u64,
    pub input_tokens: u64,
    pub context_window: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct FileInfo {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContextInfo {
    pub directory_list_bytes: u64,
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, VariantNames)]
#[ts(export)]
pub enum Model {
    ClaudeOpus46,
    ClaudeOpus45,
    ClaudeSonnet46,
    ClaudeSonnet45,
    ClaudeHaiku45,
    Gemini3ProPreview,
    Gpt52,
    Gpt51CodexMax,
    KimiK25,
    Gemini3FlashPreview,
    GLM5,
    MinimaxM25,
    Grok41Fast,
    GrokCodeFast1,
    Qwen3Coder,
    GptOss120b,
    OpenRouterAuto,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ModelInfo {
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub enum MessageSender {
    User,
    System,
    Warning,
    Error,
    Assistant { agent: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ReasoningData {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub blob: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ToolUseData {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cached_prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ImageData {
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ChatMessage {
    pub timestamp: u64,
    pub sender: MessageSender,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub reasoning: Option<ReasoningData>,
    pub tool_calls: Vec<ToolUseData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub model_info: Option<ModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub token_usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub context_breakdown: Option<ContextBreakdown>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub images: Option<Vec<ImageData>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind")]
pub enum ToolRequestType {
    ModifyFile {
        file_path: String,
        before: String,
        after: String,
    },
    RunCommand {
        command: String,
        working_directory: String,
    },
    ReadFiles {
        file_paths: Vec<String>,
    },
    SearchTypes {
        language: String,
        workspace_root: String,
        type_name: String,
    },
    GetTypeDocs {
        language: String,
        workspace_root: String,
        type_path: String,
    },
    Other {
        args: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind")]
pub enum ToolExecutionResult {
    ModifyFile {
        lines_added: u64,
        lines_removed: u64,
    },
    RunCommand {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    ReadFiles {
        files: Vec<FileInfo>,
    },
    SearchTypes {
        types: Vec<String>,
    },
    GetTypeDocs {
        documentation: String,
    },
    Error {
        short_message: String,
        detailed_message: String,
    },
    Other {
        result: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ToolRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_type: ToolRequestType,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct Task {
    pub id: u64,
    pub description: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct TaskList {
    pub title: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SessionMetadata {
    pub id: String,
    pub title: String,
    pub last_modified: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_message_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub backend_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionData {
    pub id: String,
    pub created_at: u64,
    pub last_modified: u64,
    pub messages: Vec<Value>,
    pub tracked_files: Vec<String>,
    pub events: Vec<ChatEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct StreamStartData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub message_id: Option<String>,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct StreamTextDeltaData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub message_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct StreamEndData {
    pub message: ChatMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ToolExecutionCompletedData {
    pub tool_call_id: String,
    pub tool_name: String,
    pub tool_result: ToolExecutionResult,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct OperationCancelledData {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct RetryAttemptData {
    pub attempt: u64,
    pub max_retries: u64,
    pub error: String,
    pub backoff_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SessionStartedData {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SessionsListData {
    pub sessions: Vec<SessionMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ProfilesListData {
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ModelsListEntry {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ModelsListData {
    pub models: Vec<ModelsListEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct TimingUpdateData {
    pub waiting_for_human: DurationLike,
    pub ai_processing: DurationLike,
    pub tool_execution: DurationLike,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SubprocessExitData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ModuleSchemasData {
    pub schemas: Vec<ModuleSchemaInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionSettingsData {
    #[serde(
        default,
        alias = "reviewLevel",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub review_level: Option<Option<String>>,
    #[serde(
        default,
        alias = "modelQuality",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub model_quality: Option<Option<String>>,
    #[serde(
        default,
        alias = "reasoningEffort",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub reasoning_effort: Option<Option<String>>,
    #[serde(
        default,
        alias = "communicationTone",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub communication_tone: Option<Option<String>>,
    #[serde(
        default,
        alias = "autonomyLevel",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub autonomy_level: Option<Option<String>>,
    #[serde(
        default,
        alias = "defaultAgent",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub default_agent: Option<Option<String>>,
    #[serde(
        default,
        alias = "enableTypeAnalyzer",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub enable_type_analyzer: Option<Option<bool>>,
    #[serde(
        default,
        alias = "disableStreaming",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub disable_streaming: Option<Option<bool>>,
    #[serde(
        default,
        alias = "disableCustomSteering",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub disable_custom_steering: Option<Option<bool>>,
    #[serde(
        default,
        alias = "spawnContextMode",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub spawn_context_mode: Option<Option<String>>,
    #[serde(
        default,
        alias = "runBuildTestOutputMode",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub run_build_test_output_mode: Option<Option<String>>,
    #[serde(
        default,
        alias = "active_profile",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub profile: Option<Option<String>>,
    #[serde(
        default,
        alias = "activeProvider",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub active_provider: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub providers: Option<Option<Value>>,
    #[serde(default, alias = "mcpServers", skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub mcp_servers: Option<Option<Value>>,
    #[serde(
        default,
        alias = "agentModels",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub agent_models: Option<Option<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub modules: Option<Option<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub model: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub effort: Option<Option<String>>,
    #[serde(
        default,
        alias = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub permission_mode: Option<Option<String>>,
    #[serde(
        default,
        alias = "approvalPolicy",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional = nullable)]
    pub approval_policy: Option<Option<String>>,
    #[serde(default, alias = "sessionId", skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub session_id: Option<Option<String>>,
    #[serde(default, alias = "modeId", skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub mode: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChatEventPayload {
    #[serde(rename = "conversation_id", alias = "agent_id")]
    pub conversation_id: String,
    pub event: ChatEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConversationRegisteredPayload {
    #[serde(rename = "conversation_id", alias = "agent_id")]
    pub conversation_id: String,
    pub data: ConversationRegisteredData,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AdminEventPayload {
    pub admin_id: u64,
    pub event: ChatEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, VariantNames)]
#[ts(export)]
#[serde(tag = "kind", content = "data")]
pub enum ChatEvent {
    MessageAdded(ChatMessage),
    StreamStart(StreamStartData),
    StreamDelta(StreamTextDeltaData),
    StreamReasoningDelta(StreamTextDeltaData),
    StreamEnd(StreamEndData),
    Settings(SessionSettingsData),
    TypingStatusChanged(bool),
    ConversationCleared,
    ToolRequest(ToolRequest),
    ToolExecutionCompleted(ToolExecutionCompletedData),
    OperationCancelled(OperationCancelledData),
    RetryAttempt(RetryAttemptData),
    TaskUpdate(TaskList),
    SessionStarted(SessionStartedData),
    SessionsList(SessionsListData),
    ProfilesList(ProfilesListData),
    ModelsList(ModelsListData),
    TimingUpdate(TimingUpdateData),
    ModuleSchemas(ModuleSchemasData),
    SubprocessStderr(String),
    SubprocessExit(SubprocessExitData),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, VariantNames)]
#[ts(export)]
pub enum ChatActorMessage {
    UserInput(String),
    UserInputWithImages {
        text: String,
        images: Vec<ImageData>,
    },
    ChangeProvider(String),
    GetSettings,
    SaveSettings {
        settings: SessionSettingsData,
        persist: bool,
    },
    SwitchProfile {
        profile_name: String,
    },
    SaveProfile {
        profile_name: String,
    },
    ListProfiles,
    ListSessions,
    ResumeSession {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    GetModuleSchemas,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EmptyObject {}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ImageAttachment {
    pub data: String,
    pub media_type: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "PascalCase")]
pub enum GitFileStatusKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct GitFileStatus {
    pub path: String,
    pub status: GitFileStatusKind,
    pub staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Tycode,
    Codex,
    Claude,
    Kiro,
    Gemini,
}

impl BackendKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Tycode => "tycode",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kiro => "kiro",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RuntimeAgent {
    pub agent_id: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub ui_owner_project_id: Option<String>,
    pub workspace_roots: Vec<String>,
    pub backend_kind: String,
    pub parent_agent_id: Option<String>,
    pub name: String,
    pub agent_type: Option<String>,
    pub agent_definition_id: Option<String>,
    pub is_running: bool,
    pub summary: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "mode", content = "tools")]
pub enum ToolPolicy {
    Unrestricted,
    AllowList(Vec<String>),
    DenyList(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpTransportHttp {
    #[serde(rename = "type")]
    #[ts(type = "\"http\"")]
    pub kind: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AgentMcpTransportStdio {
    #[serde(rename = "type")]
    #[ts(type = "\"stdio\"")]
    pub kind: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(untagged)]
pub enum AgentMcpTransport {
    Http(AgentMcpTransportHttp),
    Stdio(AgentMcpTransportStdio),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentMcpServer {
    pub name: String,
    pub transport: AgentMcpTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bootstrap_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_names: Vec<String>,
    pub mcp_servers: Vec<AgentMcpServer>,
    pub tool_policy: ToolPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub default_backend: Option<String>,
    pub include_agent_control: bool,
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum DefinitionScope {
    Builtin,
    Global,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentDefinitionEntry {
    #[serde(flatten)]
    pub definition: AgentDefinition,
    pub scope: DefinitionScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RuntimeAgentEvent {
    pub seq: u64,
    pub agent_id: String,
    pub conversation_id: String,
    pub kind: String,
    pub is_running: bool,
    pub timestamp_ms: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RuntimeAgentEventBatch {
    pub events: Vec<RuntimeAgentEvent>,
    pub latest_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SpawnAgentResponse {
    pub agent_id: String,
    pub conversation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentResult {
    pub agent_id: String,
    pub is_running: bool,
    pub message: Option<String>,
    pub error: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AwaitAgentsResponse {
    pub ready: Vec<AgentResult>,
    pub still_running: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CollectedAgentResult {
    pub agent: RuntimeAgent,
    pub final_message: Option<String>,
    pub changed_files: Vec<String>,
    pub tool_results: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct McpHttpServerSettings {
    pub enabled: bool,
    pub running: bool,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DriverMcpHttpServerSettings {
    pub enabled: bool,
    pub autoload: bool,
    pub running: bool,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BackendDepResult {
    pub available: bool,
    pub binary_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BackendDependencyStatus {
    pub tycode: BackendDepResult,
    pub codex: BackendDepResult,
    pub claude: BackendDepResult,
    pub kiro: BackendDepResult,
    pub gemini: BackendDepResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct DevInstanceStartParams {
    pub project_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ssh_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DevInstanceStartResult {
    pub instance_id: u64,
    pub debug_mcp_url: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DevInstanceStopParams {
    pub instance_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DevInstanceStopResult {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DevInstanceInfo {
    pub instance_id: u64,
    pub project_dir: String,
    pub ssh_host: Option<String>,
    pub agent_id: Option<String>,
    pub debug_mcp_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "type")]
pub enum WorkflowActionEntry {
    #[serde(rename = "run_command")]
    RunCommand { command: String },
    #[serde(rename = "spawn_agent")]
    SpawnAgent { prompt: String, name: String },
    #[serde(rename = "run_workflow")]
    RunWorkflow {
        #[serde(rename = "workflowId")]
        workflow_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorkflowStepEntry {
    pub name: String,
    pub actions: Vec<WorkflowActionEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorkflowEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub trigger: String,
    pub steps: Vec<WorkflowStepEntry>,
    pub scope: WorkflowScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ShellCommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionRecord {
    pub id: String,
    #[serde(default = "default_local_host_id")]
    pub host_id: String,
    pub backend_session_id: Option<String>,
    pub backend_kind: String,
    pub alias: Option<String>,
    pub user_alias: Option<String>,
    pub parent_id: Option<String>,
    pub workspace_root: Option<String>,
    pub workspace_roots: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: u64,
}

fn default_local_host_id() -> String {
    "local".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateAgentResponse {
    pub agent_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    TydeServer,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Host {
    pub id: String,
    pub label: String,
    pub hostname: String,
    pub is_local: bool,
    pub remote_kind: RemoteKind,
    pub enabled_backends: Vec<String>,
    pub default_backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RemoteControlSettings {
    pub enabled: bool,
    pub running: bool,
    pub socket_path: Option<String>,
    pub connected_clients: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RemoteServerStatus {
    pub status: String,
    pub protocol_version: u32,
    pub tyde_version: String,
    pub pid: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BackendUsageWindow {
    pub id: String,
    pub label: String,
    pub used_percent: Option<f64>,
    pub reset_at_text: Option<String>,
    pub reset_at_unix: Option<i64>,
    pub window_minutes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BackendUsageResult {
    pub backend_kind: BackendKind,
    pub source: String,
    pub captured_at_ms: u64,
    pub plan: Option<String>,
    pub status: Option<String>,
    pub windows: Vec<BackendUsageWindow>,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTydeServerState {
    NotInstalled,
    Stopped,
    RunningCurrent,
    RunningStale,
    RunningUnknown,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RemoteTydeServerStatus {
    pub host_id: String,
    pub host: String,
    pub state: RemoteTydeServerState,
    pub local_version: String,
    pub remote_version: Option<String>,
    pub target: Option<String>,
    pub socket_path: Option<String>,
    pub install_path: Option<String>,
    pub installed_versions: Vec<String>,
    pub installed_client_version: bool,
    pub running: bool,
    pub needs_upgrade: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub workspace_path: String,
    pub roots: Vec<String>,
    pub parent_project_id: Option<String>,
    pub workbench_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileChangedPayload {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TerminalOutputPayload {
    pub terminal_id: u64,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TerminalExitPayload {
    pub terminal_id: u64,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RemoteConnectionProgress {
    pub host: String,
    pub step: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ReconnectingAttempt {
    pub attempt: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DisconnectedReason {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum TydeServerConnectionScalarState {
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(untagged)]
pub enum TydeServerConnectionStateValue {
    Scalar(TydeServerConnectionScalarState),
    Reconnecting { reconnecting: ReconnectingAttempt },
    Disconnected { disconnected: DisconnectedReason },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TydeServerConnectionState {
    pub host_id: String,
    pub state: TydeServerConnectionStateValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TydeServerVersionWarning {
    pub host_id: String,
    pub host: String,
    pub local_version: String,
    pub remote_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProjectsChangedPayload {
    pub projects: Vec<ProjectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct HostsChangedPayload {
    pub hosts: Vec<Host>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateWorkbenchPayload {
    pub parent_workspace_path: String,
    pub branch: String,
    pub worktree_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeleteWorkbenchPayload {
    pub workspace_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct HostProjectsChangedPayload {
    pub host_id: String,
    pub projects: Vec<ProjectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AppBootstrap {
    pub agents: Vec<RuntimeAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TydeEventEnvelope {
    pub stream: String,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DispatchRequest {
    pub command: String,
    #[serde(default = "empty_json_object")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DesktopRequest {
    pub action: String,
    #[serde(default = "empty_json_object")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DebugRequest {
    pub action: String,
    #[serde(default = "empty_json_object")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DebugUiRequestPayload {
    #[serde(alias = "request_id")]
    pub request_id: String,
    pub action: String,
    #[serde(default = "empty_json_object")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConversationIdParams {
    #[serde(alias = "agent_id")]
    pub conversation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSessionParams {
    #[serde(alias = "agent_id")]
    pub conversation_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionRecordsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct IdParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct IdNameParams {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct IdAliasParams {
    pub id: String,
    pub alias: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct HostScopedIdParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct HostScopedIdNameParams {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct HostScopedIdAliasParams {
    pub id: String,
    pub alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsParams {
    #[serde(alias = "agent_id")]
    pub conversation_id: String,
    pub settings: SessionSettingsData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub persist: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConversationProfileParams {
    #[serde(alias = "agent_id")]
    pub conversation_id: String,
    pub profile_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct CreateAgentParams {
    pub workspace_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub backend_kind: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ephemeral: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub agent_definition_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub ui_owner_project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams {
    #[serde(alias = "agent_id")]
    pub conversation_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub images: Option<Vec<ImageAttachment>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct SpawnAgentParams {
    pub workspace_roots: Vec<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub backend_kind: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub ui_owner_project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ephemeral: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub agent_definition_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdParams {
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdMessageParams {
    pub agent_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdNameParams {
    pub agent_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WaitForAgentParams {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AgentEventsSinceParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub since_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AwaitAgentsParams {
    pub agent_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct CreateAdminSubprocessParams {
    pub workspace_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub backend_kind: Option<BackendKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AdminIdParams {
    pub admin_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AdminUpdateSettingsParams {
    pub admin_id: u64,
    pub settings: SessionSettingsData,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AdminProfileParams {
    pub admin_id: u64,
    pub profile_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AdminSessionParams {
    pub admin_id: u64,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirParams {
    pub workspace_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirParams {
    pub working_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirPathsParams {
    pub working_dir: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirMessageParams {
    pub working_dir: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirPathParams {
    pub working_dir: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirPathStagedParams {
    pub working_dir: String,
    pub path: String,
    pub staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirPathBranchParams {
    pub working_dir: String,
    pub path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ListDirectoryParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub show_hidden: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PathParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PathsParams {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathParams {
    pub workspace_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct TerminalWriteParams {
    pub terminal_id: u64,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResizeParams {
    pub terminal_id: u64,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct TerminalIdParams {
    pub terminal_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct EnabledParams {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BackendStringParams {
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct BackendKindStringParams {
    pub backend_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BackendsParams {
    pub backends: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AddHostParams {
    pub label: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IdLabelParams {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IdBackendsParams {
    pub id: String,
    pub backends: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IdBackendParams {
    pub id: String,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct HostSelectorParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AddProjectParams {
    pub workspace_path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AddProjectWorkbenchParams {
    pub parent_project_id: String,
    pub workspace_path: String,
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct HostProjectIdParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct RenameProjectParams {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProjectRootsParams {
    pub id: String,
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct RegisterGitWorkbenchParams {
    pub parent_workspace_path: String,
    pub worktree_path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct QueryBackendUsageParams {
    pub backend_kind: BackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePathOptionalParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct SaveWorkflowParams {
    pub workflow_json: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ScopedIdParams {
    pub id: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RunShellCommandParams {
    pub command: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct SaveAgentDefinitionParams {
    pub definition_json: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct HostIdParams {
    #[serde(alias = "host_id")]
    pub host_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackParams {
    pub feedback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum DialogKind {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmDialogParams {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub kind: Option<DialogKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[serde(alias = "ok_label")]
    pub ok_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[serde(alias = "cancel_label")]
    pub cancel_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ParentPathParams {
    #[serde(alias = "parent_path")]
    pub parent_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct DebugUiResponseParams {
    #[serde(alias = "request_id")]
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<String>,
}

pub struct CommandSpec {
    pub name: &'static str,
    pub params_ts: fn(&Config) -> String,
    pub response_ts: fn(&Config) -> String,
    pub desktop_only: bool,
}

pub struct ActionSpec {
    pub name: &'static str,
    pub params_ts: fn(&Config) -> String,
    pub response_ts: fn(&Config) -> String,
}

pub struct EventSpec {
    pub name: &'static str,
    pub payload_ts: fn(&Config) -> String,
    pub desktop_only: bool,
}

fn ts_params_type<T: TS>(cfg: &Config) -> String {
    let inline = <T as TS>::inline(cfg);
    if inline == "EmptyObject" || inline == "Record<symbol, never>" {
        "Record<string, never>".to_string()
    } else {
        inline
    }
}

fn ts_response_type<T: TS>(cfg: &Config) -> String {
    let inline = <T as TS>::inline(cfg);
    if inline == "null" {
        "void".to_string()
    } else if inline == "EmptyObject" || inline == "Record<symbol, never>" {
        "Record<string, never>".to_string()
    } else {
        inline
    }
}

macro_rules! command_spec {
    ($name:literal, $params:ty, $response:ty, desktop_only: $desktop:expr) => {
        CommandSpec {
            name: $name,
            params_ts: ts_params_type::<$params>,
            response_ts: ts_response_type::<$response>,
            desktop_only: $desktop,
        }
    };
}

macro_rules! action_spec {
    ($name:literal, $params:ty, $response:ty) => {
        ActionSpec {
            name: $name,
            params_ts: ts_params_type::<$params>,
            response_ts: ts_response_type::<$response>,
        }
    };
}

macro_rules! event_spec {
    ($name:literal, $payload:ty, desktop_only: $desktop:expr) => {
        EventSpec {
            name: $name,
            payload_ts: ts_response_type::<$payload>,
            desktop_only: $desktop,
        }
    };
}

pub const COMMAND_SPECS: &[CommandSpec] = &[
    command_spec!("create_agent", CreateAgentParams, CreateAgentResponse, desktop_only: false),
    command_spec!("send_message", SendMessageParams, (), desktop_only: false),
    command_spec!("cancel_conversation", ConversationIdParams, (), desktop_only: false),
    command_spec!("close_conversation", ConversationIdParams, (), desktop_only: false),
    command_spec!("list_sessions", ConversationIdParams, (), desktop_only: false),
    command_spec!("resume_session", ConversationSessionParams, (), desktop_only: false),
    command_spec!("get_session_id", ConversationIdParams, Option<String>, desktop_only: false),
    command_spec!("delete_session", ConversationSessionParams, (), desktop_only: false),
    command_spec!("list_session_records", ListSessionRecordsParams, Vec<SessionRecord>, desktop_only: false),
    command_spec!("rename_session", HostScopedIdNameParams, (), desktop_only: false),
    command_spec!("set_session_alias", HostScopedIdAliasParams, (), desktop_only: false),
    command_spec!("delete_session_record", HostScopedIdParams, (), desktop_only: false),
    command_spec!("get_settings", ConversationIdParams, (), desktop_only: false),
    command_spec!("update_settings", UpdateSettingsParams, (), desktop_only: false),
    command_spec!("list_models", ConversationIdParams, (), desktop_only: false),
    command_spec!("list_profiles", ConversationIdParams, (), desktop_only: false),
    command_spec!("switch_profile", ConversationProfileParams, (), desktop_only: false),
    command_spec!("get_module_schemas", ConversationIdParams, (), desktop_only: false),
    command_spec!("spawn_agent", SpawnAgentParams, SpawnAgentResponse, desktop_only: false),
    command_spec!("send_agent_message", AgentIdMessageParams, (), desktop_only: false),
    command_spec!("interrupt_agent", AgentIdParams, (), desktop_only: false),
    command_spec!("terminate_agent", AgentIdParams, (), desktop_only: false),
    command_spec!("rename_agent", AgentIdNameParams, (), desktop_only: false),
    command_spec!("get_agent", AgentIdParams, Option<RuntimeAgent>, desktop_only: false),
    command_spec!("list_agents", EmptyObject, Vec<RuntimeAgent>, desktop_only: false),
    command_spec!("wait_for_agent", WaitForAgentParams, RuntimeAgent, desktop_only: false),
    command_spec!("agent_events_since", AgentEventsSinceParams, RuntimeAgentEventBatch, desktop_only: false),
    command_spec!("collect_agent_result", AgentIdParams, CollectedAgentResult, desktop_only: false),
    command_spec!("cancel_agent", AgentIdParams, AgentResult, desktop_only: false),
    command_spec!("create_admin_subprocess", CreateAdminSubprocessParams, u64, desktop_only: false),
    command_spec!("close_admin_subprocess", AdminIdParams, (), desktop_only: false),
    command_spec!("admin_list_sessions", AdminIdParams, (), desktop_only: false),
    command_spec!("admin_get_settings", AdminIdParams, (), desktop_only: false),
    command_spec!("admin_update_settings", AdminUpdateSettingsParams, (), desktop_only: false),
    command_spec!("admin_list_profiles", AdminIdParams, (), desktop_only: false),
    command_spec!("admin_switch_profile", AdminProfileParams, (), desktop_only: false),
    command_spec!("admin_get_module_schemas", AdminIdParams, (), desktop_only: false),
    command_spec!("admin_delete_session", AdminSessionParams, (), desktop_only: false),
    command_spec!("discover_git_repos", WorkspaceDirParams, Vec<String>, desktop_only: true),
    command_spec!("git_current_branch", WorkingDirParams, String, desktop_only: false),
    command_spec!("git_status", WorkingDirParams, Vec<GitFileStatus>, desktop_only: false),
    command_spec!("git_stage", WorkingDirPathsParams, (), desktop_only: false),
    command_spec!("git_unstage", WorkingDirPathsParams, (), desktop_only: false),
    command_spec!("git_commit", WorkingDirMessageParams, String, desktop_only: false),
    command_spec!("git_diff", WorkingDirPathStagedParams, String, desktop_only: false),
    command_spec!("git_diff_base_content", WorkingDirPathStagedParams, String, desktop_only: false),
    command_spec!("git_discard", WorkingDirPathsParams, (), desktop_only: false),
    command_spec!("git_worktree_add", WorkingDirPathBranchParams, (), desktop_only: true),
    command_spec!("git_worktree_remove", WorkingDirPathParams, (), desktop_only: true),
    command_spec!("list_directory", ListDirectoryParams, Vec<FileEntry>, desktop_only: false),
    command_spec!("read_file_content", PathParams, FileContent, desktop_only: false),
    command_spec!("sync_file_watch_paths", PathsParams, (), desktop_only: true),
    command_spec!("watch_workspace_dir", PathParams, (), desktop_only: true),
    command_spec!("unwatch_workspace_dir", EmptyObject, (), desktop_only: true),
    command_spec!("create_terminal", WorkspacePathParams, u64, desktop_only: true),
    command_spec!("write_terminal", TerminalWriteParams, (), desktop_only: true),
    command_spec!("resize_terminal", TerminalResizeParams, (), desktop_only: true),
    command_spec!("close_terminal", TerminalIdParams, (), desktop_only: true),
    command_spec!("get_mcp_http_server_settings", EmptyObject, McpHttpServerSettings, desktop_only: true),
    command_spec!("set_mcp_http_server_enabled", EnabledParams, McpHttpServerSettings, desktop_only: true),
    command_spec!("set_mcp_control_enabled", EnabledParams, (), desktop_only: true),
    command_spec!("get_driver_mcp_http_server_settings", EmptyObject, DriverMcpHttpServerSettings, desktop_only: true),
    command_spec!("set_driver_mcp_http_server_enabled", EnabledParams, DriverMcpHttpServerSettings, desktop_only: true),
    command_spec!("set_driver_mcp_http_server_autoload_enabled", EnabledParams, DriverMcpHttpServerSettings, desktop_only: true),
    command_spec!("list_hosts", EmptyObject, Vec<Host>, desktop_only: true),
    command_spec!("add_host", AddHostParams, Host, desktop_only: true),
    command_spec!("remove_host", IdParams, (), desktop_only: true),
    command_spec!("update_host_label", IdLabelParams, (), desktop_only: true),
    command_spec!("update_host_enabled_backends", IdBackendsParams, (), desktop_only: true),
    command_spec!("update_host_default_backend", IdBackendParams, (), desktop_only: true),
    command_spec!("get_host_for_workspace", WorkspacePathParams, Host, desktop_only: true),
    command_spec!("set_remote_control_enabled", EnabledParams, RemoteControlSettings, desktop_only: true),
    command_spec!("get_remote_control_settings", EmptyObject, RemoteControlSettings, desktop_only: true),
    command_spec!("server_status", EmptyObject, RemoteServerStatus, desktop_only: false),
    command_spec!("query_backend_usage", QueryBackendUsageParams, BackendUsageResult, desktop_only: true),
    command_spec!("check_backend_dependencies", EmptyObject, BackendDependencyStatus, desktop_only: true),
    command_spec!("set_disabled_backends", BackendsParams, (), desktop_only: true),
    command_spec!("install_backend_dependency", BackendKindStringParams, (), desktop_only: true),
    command_spec!("restart_subprocess", ConversationIdParams, (), desktop_only: false),
    command_spec!("relaunch_conversation", ConversationIdParams, (), desktop_only: false),
    command_spec!("list_active_conversations", EmptyObject, Vec<String>, desktop_only: true),
    command_spec!("list_projects", HostSelectorParams, Vec<ProjectRecord>, desktop_only: false),
    command_spec!("add_project", AddProjectParams, ProjectRecord, desktop_only: false),
    command_spec!("add_project_workbench", AddProjectWorkbenchParams, ProjectRecord, desktop_only: false),
    command_spec!("remove_project", HostProjectIdParams, (), desktop_only: false),
    command_spec!("remove_project_by_workspace_path", WorkspacePathParams, (), desktop_only: true),
    command_spec!("rename_project", RenameProjectParams, (), desktop_only: false),
    command_spec!("update_project_roots", UpdateProjectRootsParams, (), desktop_only: false),
    command_spec!("register_git_workbench", RegisterGitWorkbenchParams, (), desktop_only: true),
    command_spec!("set_default_backend", BackendStringParams, (), desktop_only: true),
    command_spec!("list_workflows", WorkspacePathOptionalParams, Vec<WorkflowEntry>, desktop_only: true),
    command_spec!("save_workflow", SaveWorkflowParams, (), desktop_only: true),
    command_spec!("delete_workflow", ScopedIdParams, (), desktop_only: true),
    command_spec!("run_shell_command", RunShellCommandParams, ShellCommandResult, desktop_only: true),
    command_spec!("list_agent_definitions", WorkspacePathOptionalParams, Vec<AgentDefinitionEntry>, desktop_only: true),
    command_spec!("save_agent_definition", SaveAgentDefinitionParams, (), desktop_only: true),
    command_spec!("delete_agent_definition", ScopedIdParams, (), desktop_only: true),
    command_spec!("list_available_skills", WorkspacePathOptionalParams, Vec<String>, desktop_only: true),
];

pub const DESKTOP_ACTION_SPECS: &[ActionSpec] = &[
    action_spec!("get_initial_workspace", EmptyObject, Option<String>),
    action_spec!("open_workspace_dialog", EmptyObject, Option<String>),
    action_spec!("pick_sub_root_dialog", ParentPathParams, Option<String>),
    action_spec!("confirm", ConfirmDialogParams, bool),
    action_spec!("reveal_item_in_dir", PathParams, ()),
    action_spec!("shutdown_all_subprocesses", EmptyObject, ()),
    action_spec!("submit_feedback", FeedbackParams, ()),
    action_spec!("replay_host_state", EmptyObject, ()),
    action_spec!(
        "get_remote_tyde_server_status",
        HostIdParams,
        RemoteTydeServerStatus
    ),
    action_spec!(
        "install_remote_tyde_server",
        HostIdParams,
        RemoteTydeServerStatus
    ),
    action_spec!(
        "launch_remote_tyde_server",
        HostIdParams,
        RemoteTydeServerStatus
    ),
    action_spec!(
        "install_and_launch_remote_tyde_server",
        HostIdParams,
        RemoteTydeServerStatus
    ),
    action_spec!(
        "upgrade_remote_tyde_server",
        HostIdParams,
        RemoteTydeServerStatus
    ),
];

pub const DEBUG_ACTION_SPECS: &[ActionSpec] = &[action_spec!(
    "submit_ui_response",
    DebugUiResponseParams,
    ()
)];

pub const EVENT_SPECS: &[EventSpec] = &[
    event_spec!("chat-event", ChatEventPayload, desktop_only: false),
    event_spec!("chat-registered", ConversationRegisteredPayload, desktop_only: false),
    event_spec!("agent-changed", RuntimeAgent, desktop_only: true),
    event_spec!("admin-event", AdminEventPayload, desktop_only: false),
    event_spec!("file-changed", FileChangedPayload, desktop_only: false),
    event_spec!("terminal-output", TerminalOutputPayload, desktop_only: true),
    event_spec!("terminal-exit", TerminalExitPayload, desktop_only: true),
    event_spec!("hosts-changed", HostsChangedPayload, desktop_only: true),
    event_spec!("projects-changed", HostProjectsChangedPayload, desktop_only: true),
    event_spec!("remote-connection-progress", RemoteConnectionProgress, desktop_only: true),
    event_spec!("tyde-server-connection-state", TydeServerConnectionState, desktop_only: true),
    event_spec!("tyde-server-version-warning", TydeServerVersionWarning, desktop_only: true),
    event_spec!("create-workbench", CreateWorkbenchPayload, desktop_only: true),
    event_spec!("delete-workbench", DeleteWorkbenchPayload, desktop_only: true),
];

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{ChatEvent, ClientFrame, HandshakeResult, PROTOCOL_VERSION};

    #[test]
    fn handshake_roundtrips_chat_cursor_map() {
        let mut cursors = HashMap::new();
        cursors.insert("42".to_string(), 7);
        let frame = ClientFrame::Handshake {
            req_id: 0,
            protocol_version: PROTOCOL_VERSION,
            tyde_version: "0.0.0".to_string(),
            last_agent_event_seq: 3,
            last_chat_event_seqs: cursors.clone(),
        };
        let json = serde_json::to_string(&frame).expect("serialize handshake");
        let parsed: ClientFrame = serde_json::from_str(&json).expect("deserialize handshake");

        match parsed {
            ClientFrame::Handshake {
                req_id,
                protocol_version,
                tyde_version,
                last_agent_event_seq,
                last_chat_event_seqs,
            } => {
                assert_eq!(req_id, 0);
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(tyde_version, "0.0.0");
                assert_eq!(last_agent_event_seq, 3);
                assert_eq!(last_chat_event_seqs, cursors);
            }
            _ => panic!("expected handshake frame"),
        }
    }

    #[test]
    fn invoke_req_id_accepts_string_or_number() {
        let as_string = r#"{"type":"Invoke","req_id":"1","command":"list_agents","params":{}}"#;
        let parsed: ClientFrame = serde_json::from_str(as_string).expect("parse string req_id");
        match parsed {
            ClientFrame::Invoke { req_id, .. } => assert_eq!(req_id, 1),
            _ => panic!("expected invoke frame"),
        }

        let as_number = r#"{"type":"Invoke","req_id":2,"command":"list_agents","params":{}}"#;
        let parsed: ClientFrame = serde_json::from_str(as_number).expect("parse numeric req_id");
        match parsed {
            ClientFrame::Invoke { req_id, .. } => assert_eq!(req_id, 2),
            _ => panic!("expected invoke frame"),
        }
    }

    #[test]
    fn handshake_backfills_missing_tyde_version() {
        let json = r#"{
            "type":"Handshake",
            "req_id":0,
            "protocol_version":1,
            "last_agent_event_seq":0,
            "last_chat_event_seqs":{}
        }"#;

        let parsed: ClientFrame =
            serde_json::from_str(json).expect("deserialize handshake without tyde_version");

        match parsed {
            ClientFrame::Handshake { tyde_version, .. } => assert!(tyde_version.is_empty()),
            _ => panic!("expected handshake frame"),
        }
    }

    #[test]
    fn handshake_result_backfills_missing_tyde_version() {
        let json = r#"{
            "protocol_version":1,
            "agents":[],
            "conversations":[]
        }"#;

        let parsed: HandshakeResult =
            serde_json::from_str(json).expect("deserialize handshake result without tyde_version");

        assert!(parsed.tyde_version.is_empty());
    }

    #[test]
    fn conversation_cleared_has_no_data_field() {
        let event = ChatEvent::ConversationCleared;
        let json = serde_json::to_value(event).expect("serialize chat event");

        assert_eq!(json, serde_json::json!({ "kind": "ConversationCleared" }));
    }
}
