use crate::types::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct HumanInputId(pub String);
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct LearningChangeId(pub String);
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputSource {
    Chat,
    Steering,
    AskUserAnswer,
    Tychat,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanInputPayload {
    pub id: HumanInputId,
    pub action: HumanInputAction,
    pub input: SendMessagePayload,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputAction {
    Send,
    Steer,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedHumanInput {
    pub id: HumanInputId,
    pub source: HumanInputSource,
    pub agent_id: AgentId,
    pub session_id: Option<SessionId>,
    pub project_id: Option<ProjectId>,
    pub message: String,
    pub assistant_context: String,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LearningMode {
    #[default]
    Suggest,
    Automatic,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LearningAnalyzer {
    pub backend_kind: BackendKind,
    pub model: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LearningConfig {
    pub enabled: bool,
    pub mode: LearningMode,
    pub analyzer: Option<LearningAnalyzer>,
    pub steering: bool,
    pub review_aspects: bool,
}
impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: LearningMode::Suggest,
            analyzer: None,
            steering: true,
            review_aspects: true,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SelfImprovementSettings {
    pub host: LearningConfig,
    pub projects: std::collections::BTreeMap<ProjectId, LearningConfig>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningRule {
    Steering {
        steering: Steering,
    },
    ReviewAspect {
        id: String,
        aspect: ReviewAspectConfig,
        scope: SteeringScope,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningOperation {
    pub before: Option<LearningRule>,
    pub after: LearningRule,
    pub reason: String,
    pub origin_id: HumanInputId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningAnalysis {
    pub operations: Vec<LearningOperation>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningChangeStatus {
    Pending,
    Applying,
    Applied,
    Undoing,
    Dismissed,
    Undone,
    Conflict { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningChange {
    pub id: LearningChangeId,
    pub scope: SteeringScope,
    pub operation: LearningOperation,
    pub origin_agent_id: AgentId,
    pub origin_message: String,
    pub created_at_ms: u64,
    pub status: LearningChangeStatus,
    pub requires_approval: bool,
    pub expected_revision: u64,
    pub applied_revision: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningStatus {
    Disabled,
    Ready,
    Running,
    Unsupported { reason: String },
    Failed { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningUsage {
    pub scope: SteeringScope,
    pub batch_id: LearningChangeId,
    pub origin_agent_ids: Vec<AgentId>,
    pub origin_session_ids: Vec<SessionId>,
    pub input_ids: Vec<HumanInputId>,
    pub analyzer: LearningAnalyzer,
    pub usage: TaskTokenUsageScope,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningScopeState {
    pub scope: SteeringScope,
    pub status: LearningStatus,
    pub queued: usize,
    pub last_checked_ms: Option<u64>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfImprovementState {
    #[serde(default)]
    pub canonical_revision: u64,
    pub analyzers: Vec<LearningAnalyzerOption>,
    pub scopes: Vec<LearningScopeState>,
    pub changes: Vec<LearningChange>,
    pub usage: Vec<LearningUsage>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SelfImprovementAction {
    Apply {
        id: LearningChangeId,
    },
    Edit {
        id: LearningChangeId,
        after: LearningRule,
        reason: String,
    },
    Dismiss {
        id: LearningChangeId,
    },
    Undo {
        id: LearningChangeId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningBatch {
    pub id: LearningChangeId,
    pub inputs: Vec<AcceptedHumanInput>,
    pub config: LearningConfig,
    pub revisions: std::collections::BTreeMap<String, u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningJob {
    pub state: LearningScopeState,
    pub pending: Vec<AcceptedHumanInput>,
    pub active: Option<LearningBatch>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningJournal {
    #[serde(default)]
    pub canonical_revision: u64,
    pub seen: std::collections::HashSet<HumanInputId>,
    pub jobs: Vec<LearningJob>,
    pub changes: Vec<LearningChange>,
    pub usage: Vec<LearningUsage>,
    pub revisions: std::collections::BTreeMap<String, u64>,
    pub managed: std::collections::BTreeMap<String, LearningRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningAnalyzerCapability {
    ToolFreeReusable,
    Unsupported { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningAnalyzerOption {
    pub backend_kind: BackendKind,
    pub capability: LearningAnalyzerCapability,
}
