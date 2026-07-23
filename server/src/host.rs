use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
#[cfg(feature = "test-support")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow};
use futures_util::FutureExt;
use protocol::types::{
    AgentClosedPayload, AgentCompactNotifyPayload, AgentCompactPayload, AgentCompactStatus,
    TeamCompactNotifyPayload, TeamCompactPayload, TeamCompactStatus,
};
use protocol::{
    AgentActivitySummary, AgentActivitySummaryPayload, AgentActivitySummaryStaleReason,
    AgentActivitySummaryState, AgentAnnotationTarget, AgentControlStatus, AgentGroupsUpdate,
    AgentId, AgentInput, AgentOrderKey, AgentOrigin, AgentPinsUpdate, AgentStartPayload,
    AgentSystemTagAssignment, AgentSystemTagDescriptor, AgentSystemTagId, AgentTagsSnapshot,
    AgentTagsUpdate, AgentWorkflowMetadata, AgentsViewPreferencesNotifyPayload,
    AgentsViewPreferencesSnapshot, AgentsViewPreferencesUpdate, BackendCapacityPayload,
    BackendCapacitySnapshot, BackendCapacityState, BackendConfigSnapshot,
    BackendConfigSnapshotsPayload, BackendKind, BackendNativeSettingsSnapshot, BackendSetupPayload,
    BrowseBootstrapListing, BrowseBootstrapPayload, CancelWorkflowPayload, ChatEvent, ChatMessage,
    CodeIntelCancelReferencesPayload, CodeIntelFindReferencesPayload, CodeIntelHoverPayload,
    CodeIntelNavigatePayload, CodeIntelSetVisibleRangePayload, CodeIntelSubscribeFilePayload,
    CodeIntelUnsubscribeFilePayload, CustomAgent, CustomAgentDeletePayload,
    CustomAgentNotifyPayload, CustomAgentUpsertPayload, DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT,
    FrameKind, GitBranchName, HostAbsPath, HostBootstrapPayload, HostBrowseInitial,
    HostBrowseListPayload, HostBrowseStartPayload, HostFilterId, HostLaunchProfileConfig,
    HostSettingsPayload, ImageData, LOCAL_HOST_ID, LaunchProfile, LaunchProfileCatalog,
    LaunchProfileCatalogPayload, LaunchProfileEntry, LaunchProfileId, LaunchProfileKind,
    ListSessionsPayload, MAX_SESSION_LIST_PAGE_LIMIT, McpServerConfig, McpServerDeletePayload,
    McpServerId, McpServerNotifyPayload, McpServerUpsertPayload, McpTransportConfig, MessageOrigin,
    MessageSender, MobileDeviceRenamePayload, MobileDeviceRevokePayload,
    MobilePairingCancelPayload, NewAgentPayload, Project, ProjectAddRootPayload,
    ProjectCreatePayload, ProjectDeletePayload, ProjectDeleteRootPayload,
    ProjectDiscardFilePayload, ProjectGitCommitPayload, ProjectGitCommitResultPayload, ProjectId,
    ProjectListDirPayload, ProjectNotifyPayload, ProjectPath, ProjectReadDiffPayload,
    ProjectReadFilePayload, ProjectRenamePayload, ProjectReorderPayload, ProjectRootPath,
    ProjectSearchCancelPayload, ProjectSearchCompletePayload, ProjectSearchFileResult,
    ProjectSearchPayload, ProjectSearchResultsPayload, ProjectSource, ProjectStageFilePayload,
    ProjectStageHunkPayload, ProjectUnstageFilePayload, ReviewActionPayload, ReviewCreatePayload,
    ReviewDiffSelection, ReviewId, ReviewSubmitTarget, RunBackendSetupPayload,
    SUPERVISOR_MESSAGE_PREFIX, SendMessagePayload, SessionHistoryPayload, SessionId,
    SessionListCursor, SessionListGeneration, SessionListPageInfo, SessionListPageStatus,
    SessionListPayload, SessionListScope, SessionSchemaEntry, SessionSchemasPayload,
    SessionSettingsSchema, SessionSummary, SetAgentGroupsPayload, SetAgentPinsPayload,
    SetAgentTagsPayload, SetAgentsSmartViewsPayload, SetAgentsViewPreferencesPayload,
    SetSettingPayload, Skill, SkillNotifyPayload, SkillRefreshPayload, SpawnAgentParams,
    SpawnAgentPayload, SteeringDeletePayload, SteeringNotifyPayload, SteeringScope,
    SteeringUpsertPayload, StreamPath, TaskTokenUsageAggregate, TaskTokenUsageAmount,
    TaskTokenUsageEntry, TaskTokenUsagePayload, TaskTokenUsageScope, TaskTokenUsageStatus,
    TaskTokenUsageUnavailableReason, TeamCreatePayload, TeamDeletePayload,
    TeamDraftApplyTemplatePayload, TeamDraftCommitPayload, TeamDraftCreatePayload,
    TeamDraftDiscardPayload, TeamDraftNotifyPayload, TeamDraftShufflePayload,
    TeamDraftUpdatePayload, TeamId, TeamMember, TeamMemberBindingNotifyPayload,
    TeamMemberCreatePayload, TeamMemberDeletePayload, TeamMemberId, TeamMemberNotifyPayload,
    TeamMemberRole, TeamMemberShufflePayload, TeamMemberShuffleSuggestionNotifyPayload,
    TeamMemberState, TeamMemberUpdatePayload, TeamNotifyPayload, TeamRenamePayload,
    TeamSetManagerPayload, TerminalCreatePayload, TerminalId, TerminalLaunchTarget,
    TerminalResizePayload, TerminalSendPayload, TriggerWorkflowPayload, WorkbenchCreatePayload,
    WorkbenchRemovePayload, WorkbenchRoot, WorkflowCatalogLocation, WorkflowDiagnostic,
    WorkflowDiagnosticSeverity, WorkflowInputControl, WorkflowInputSpec, WorkflowNotifyPayload,
    WorkflowRunId, WorkflowRunNotifyPayload, WorkflowRunSnapshot, WorkflowRunSnapshotStatus,
    WorkflowSaveMode, WorkflowSaveRequest, WorkflowSaveResponse, WorkflowSaveTarget,
    WorkflowSource, WorkflowSourceScope, WorkflowStepRunId, WorkflowStepRunSnapshot,
    WorkflowTargetDirectory, WorkflowTargetsResponse,
};
use tokio::sync::{Mutex, Semaphore, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::customization::{
    ResolveSpawnConfigRequest, ResolvedSpawnConfig, protocol_mcp_servers_to_startup,
    resolve_spawn_config,
};
use crate::agent::registry::{
    AgentRegistry, AgentStartupFailure, InitialAgentAlias, InitialAgentAliasPersistence,
    RelaySpawnRequest, ResolvedSpawnRequest,
};
use crate::agent::{
    AgentHandle, AgentUsageSnapshot, AppendSupervisorWarningOutcome, CompactionStart,
    CompactionSummary, DEFAULT_COMPACTION_SUMMARY_MAX_BYTES, GenerateAgentActivitySummaryRequest,
    GenerateAgentNameRequest, InterruptOutcome, MAX_COMPACTION_SUMMARY_BYTES,
    SupervisorVerdictStart, derive_agent_name, generate_agent_activity_summary,
    generate_agent_name,
};
use crate::agent_control_mcp::AgentControlMcpHandle;
use crate::backend::setup;
use crate::backend::{
    BackendSession, StartupMcpServer, StartupMcpTransport, apply_session_settings_update,
    sanitize_session_settings_values, session_settings_schema_for_backend,
    validate_session_settings_values,
};

#[derive(Clone, Debug)]
pub(crate) struct BaseRevision(pub(crate) String);

#[derive(Clone, Debug)]
pub(crate) struct CreatedWorkbenchRoot {
    pub(crate) root: WorkbenchRoot,
    pub(crate) base_commit: String,
    pub(crate) parent_root_dirty: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CreatedWorkbench {
    pub(crate) project: Project,
    pub(crate) roots: Vec<CreatedWorkbenchRoot>,
}
use crate::browse_stream;
use crate::code_intel::CodeIntelRouter;
use crate::config_mcp::ConfigMcpHandle;
use crate::debug_mcp::DebugMcpHandle;
use crate::error::{AppError, AppResult};
use crate::mobile_access::{
    MobileAccessCommand, MobileAccessHandle, MobileAccessInit, spawn_mobile_access_actor,
};
use crate::project_stream::{
    ProjectDiffRequestKey, ProjectStreamHandle, ProjectStreamSubscription, SearchSummary,
    build_dir_listing, commit, discard_file, is_not_git_repository_error, read_diff,
    search_project, spawn_project_subscription, stage_file, stage_hunk, unstage_file,
};
use crate::review::actor::{ReviewAiSpawnRequest, ReviewDeliveryOutcome, ReviewDeliveryRequest};
use crate::review::reviewer::{
    ReviewerToolBridge, build_reviewer_system_prompt, build_reviewer_user_prompt,
    reviewer_tool_policy,
};
use crate::review::{
    ReviewRegistry, ReviewRegistryHandle, build_create_request, review_create_selection,
    review_stream_path,
};
use crate::review_mcp::{REVIEW_FEEDBACK_MCP_SERVER_NAME, ReviewMcpHandle};
use crate::store::agent_teams::{AgentTeamValidationRefs, AgentTeamsStore};
use crate::store::agents_view_preferences::AgentsViewPreferencesStore;
use crate::store::custom_agents::CustomAgentStore;
use crate::store::mcp_servers::{McpServerStore, RESERVED_MCP_SERVER_NAMES};
use crate::store::mobile_pairings::MobilePairingsStore;
use crate::store::project::{ProjectStore, ProjectStoreError};
use crate::store::review::ReviewStore;
use crate::store::session::{
    SessionRecord, SessionStore, session_record_is_resumable, session_summary_matches_scope,
};
use crate::store::settings::HostSettingsStore;
use crate::store::skills::SkillStore;
use crate::store::steering::SteeringStore;
use crate::stream::{Stream, StreamClosed};
use crate::sub_agent::{
    HostSubAgentSpawnRequest, HostSubAgentSpawnRx, HostSubAgentSpawnTx, SubAgentEmitter,
    SubAgentHandle,
};

pub(crate) type HostCapacityTx = mpsc::UnboundedSender<HostCapacityUpdate>;
type HostCapacityRx = mpsc::UnboundedReceiver<HostCapacityUpdate>;

pub(crate) enum HostCapacityUpdate {
    Report {
        backend_kind: BackendKind,
        state: BackendCapacityState,
    },
    #[cfg(feature = "test-support")]
    Barrier(oneshot::Sender<()>),
}

use crate::team_registry::{
    TeamDescribeData, TeamMemberActivation, TeamMessagePlan, TeamRegistryEvents,
    TeamRegistryHandle, TeamRegistrySnapshot, team_preset_validation_refs,
};
use crate::terminal_stream::{
    TerminalHandle, TerminalLaunchCommand, TerminalLaunchInfo, create_terminal,
};
use crate::workflows::mcp::{
    WORKFLOW_PROGRESS_MCP_SERVER_NAME, WorkflowFinishToolInput, WorkflowMcpHandle,
    WorkflowReportStepToolInput,
};
use crate::workflows::registry::{
    WorkflowCatalog, global_workflows_dir, parse_workflow_content, parse_workflow_file,
    project_workflows_dir, workflow_catalog_locations, workflow_watch_dirs,
};
use crate::workflows::store::WorkflowRunStore;
use crate::workflows::watch::{WorkflowCatalogSignal, WorkflowWatcherHandle};

struct HostSubscriber {
    stream: Stream,
    bootstrapped: bool,
    agent_replay: AgentReplayMode,
    session_list_replay: SessionListReplayMode,
    session_list_snapshot: Option<SessionListSnapshot>,
    known_agent_streams: HashSet<StreamPath>,
    attached_agent_streams: HashSet<StreamPath>,
    bootstrapped_agent_streams: HashSet<StreamPath>,
    pending_bootstrap_new_agents: Vec<PendingNewAgentFanout>,
    pending_bootstrap_frames: Vec<(FrameKind, serde_json::Value)>,
    last_session_schemas: Option<Vec<SessionSchemaEntry>>,
    last_backend_config_schemas: Option<Vec<protocol::BackendConfigSchema>>,
    last_backend_config_snapshots: Option<Vec<BackendConfigSnapshot>>,
    last_backend_native_settings_snapshots: Option<Vec<BackendNativeSettingsSnapshot>>,
    last_backend_capacity: Option<Vec<BackendCapacitySnapshot>>,
    capacity_replay_ready: bool,
    last_launch_profile_catalog: Option<LaunchProfileCatalog>,
}

struct PendingNewAgentFanout {
    start: AgentStartPayload,
    agent_handle: AgentHandle,
    instance_stream: StreamPath,
    attach_eagerly: bool,
    activity_summary: AgentActivitySummaryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendSetupRefreshStep {
    Setup,
    SessionSchemas,
    BackendConfigSnapshots,
}

const BACKEND_SETUP_REFRESH_ORDER: [BackendSetupRefreshStep; 3] = [
    BackendSetupRefreshStep::Setup,
    BackendSetupRefreshStep::SessionSchemas,
    BackendSetupRefreshStep::BackendConfigSnapshots,
];

pub(crate) struct DeferredAgentAttachment {
    host_stream: StreamPath,
    agent_stream: StreamPath,
    reply: Option<oneshot::Receiver<bool>>,
    agent_handle: Option<AgentHandle>,
    stream: Option<Stream>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentReplayMode {
    Eager,
    Lazy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionListReplayMode {
    Full,
    Paged { limit: u32 },
}

impl SessionListReplayMode {
    fn for_agent_replay(agent_replay: AgentReplayMode) -> Self {
        match agent_replay {
            AgentReplayMode::Eager => Self::Full,
            AgentReplayMode::Lazy => Self::Paged {
                limit: DEFAULT_MOBILE_SESSION_LIST_PAGE_LIMIT,
            },
        }
    }

    fn default_scope(self) -> SessionListScope {
        match self {
            Self::Full => SessionListScope::AllSessions,
            Self::Paged { .. } => SessionListScope::RootSessions,
        }
    }
}

#[derive(Clone, Debug)]
struct SessionListSnapshot {
    generation: SessionListGeneration,
    scope: SessionListScope,
    sessions: Vec<SessionSummary>,
}

#[derive(Clone, Debug)]
pub struct HostRuntimeConfig {
    pub debug_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub agent_control_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub review_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub workflow_mcp_bind_addr: Option<std::net::SocketAddr>,
    pub antigravity_conversations_dir: Option<PathBuf>,
    pub codex_probe_program: Option<String>,
    pub kiro_probe_program: Option<String>,
    pub kiro_probe_workspace_root: Option<PathBuf>,
    pub mobile_pairing_ttl: Option<std::time::Duration>,
    pub mobile_managed_service_base_url: Option<String>,
    /// Skip probing real backend CLIs and Codex model metadata. Tests set this
    /// so backend setup returns an empty stub and Codex's dynamic session
    /// schema becomes explicitly unavailable unless `codex_probe_program` is
    /// supplied. Defaults to `false` so production startup is unaffected.
    pub skip_real_backend_probe: bool,
    pub agents_view_preferences_primary: bool,
    #[cfg(test)]
    pub start_agent_supervisor_worker: bool,
}

impl Default for HostRuntimeConfig {
    fn default() -> Self {
        Self {
            debug_mcp_bind_addr: None,
            agent_control_mcp_bind_addr: None,
            review_mcp_bind_addr: None,
            workflow_mcp_bind_addr: None,
            antigravity_conversations_dir: None,
            codex_probe_program: None,
            kiro_probe_program: None,
            kiro_probe_workspace_root: None,
            mobile_pairing_ttl: None,
            mobile_managed_service_base_url: None,
            skip_real_backend_probe: false,
            agents_view_preferences_primary: true,
            #[cfg(test)]
            start_agent_supervisor_worker: true,
        }
    }
}

#[derive(Clone, Debug)]
struct TeamSpawnContext {
    team_id: TeamId,
    team_member_id: TeamMemberId,
}

#[derive(Debug)]
struct ReviewTargetAgentRequest {
    review_id: ReviewId,
    project_id: ProjectId,
    backend_kind: protocol::BackendKind,
    cost_hint: Option<protocol::SpawnCostHint>,
    custom_agent_id: Option<protocol::CustomAgentId>,
    name: Option<String>,
    instructions: Option<String>,
    payload: SendMessagePayload,
}

const COMPACTION_SUMMARY_PREVIEW_CHARS: usize = 512;
const ACTIVITY_SUMMARY_HISTORY_EVENTS: usize = 40;
const ACTIVITY_SUMMARY_HISTORY_BYTES: usize = 16 * 1024;
const ACTIVITY_SUMMARY_INITIAL_DELAY: Duration = Duration::from_secs(10);
const ACTIVITY_SUMMARY_DEBOUNCE: Duration = Duration::from_secs(5);
const ACTIVITY_SUMMARY_MAX_FREQUENCY: Duration = Duration::from_secs(60);
const ACTIVITY_SUMMARY_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const ACTIVITY_SUMMARY_GENERATION_TIMEOUT: Duration = Duration::from_secs(30);
const AGENT_NAME_GENERATION_TIMEOUT: Duration = Duration::from_secs(30);
/// One supervision verdict reads more context than naming, so it gets a
/// longer budget per attempt.
const SUPERVISION_GENERATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Grace period between observing an idle transition and reading the
/// supervision context, so queued-message drains and immediate user
/// follow-ups win the race instead of being second-guessed.
const SUPERVISION_DEBOUNCE: Duration = Duration::from_secs(3);
const SUPERVISION_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(480),
];
const _: () =
    assert!(SUPERVISION_RETRY_DELAYS.len() == protocol::SUPERVISOR_RETRY_ATTEMPTS_MAX as usize);
/// How long the supervisor waits for an auto-compaction it requested to
/// reach a terminal notify before giving up on observing it (the compaction
/// itself keeps running server-side either way).
const SUPERVISION_COMPACTION_OBSERVE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActivitySummarySettingsSignal {
    enabled: bool,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SupervisorSettingsSignal {
    pub(crate) settings: protocol::SupervisorSettings,
    pub(crate) epoch: u64,
}

struct SupervisorSchedulerEntry {
    last_activity_counter: u64,
    phase: SupervisorPhase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SupervisionBaseline {
    last_user_message: String,
    kicks_since_user_message: u32,
    session_id: Option<SessionId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VerdictSettingsFingerprint {
    max_kicks_per_task: u8,
    retry_attempts: u8,
    cost_tier: protocol::SupervisorCostTier,
}

impl From<protocol::SupervisorSettings> for VerdictSettingsFingerprint {
    fn from(settings: protocol::SupervisorSettings) -> Self {
        Self {
            max_kicks_per_task: settings.max_kicks_per_task,
            retry_attempts: settings.retry_attempts,
            cost_tier: settings.cost_tier,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SupervisionRetryReason {
    Failure(crate::agent::supervisor::SupervisionFailureKind),
    SettingsChanged,
}

enum SupervisorPhase {
    Active,
    Debouncing {
        idle_since: Instant,
    },
    VerdictInFlight {
        idle_since: Instant,
        baseline: SupervisionBaseline,
        attempts_started: u8,
        verdict_settings: VerdictSettingsFingerprint,
    },
    RetryPending {
        idle_since: Instant,
        baseline: SupervisionBaseline,
        attempts_started: u8,
        due_at: Instant,
        last_failure_kind: SupervisionRetryReason,
        verdict_settings: VerdictSettingsFingerprint,
    },
    FailureExhausted {
        idle_since: Instant,
        baseline: SupervisionBaseline,
        attempts_started: u8,
        retry_due_at: Option<Instant>,
        last_failure_kind: crate::agent::supervisor::SupervisionFailureKind,
    },
    DoneAuthorized {
        idle_since: Instant,
        baseline: SupervisionBaseline,
        last_gate_evaluation_epoch: Option<u64>,
    },
    AwaitingUser {
        idle_since: Instant,
    },
    Dormant {
        idle_since: Instant,
    },
    CompactionPending {
        idle_since: Instant,
    },
    Compacting,
}

#[derive(Default)]
struct SupervisorVerdictTaskState {
    active_task_id: Option<u64>,
    next_task_id: u64,
}

impl SupervisorVerdictTaskState {
    fn reserve(&mut self) -> Option<u64> {
        if self.active_task_id.is_some() {
            return None;
        }
        let task_id = self.next_task_id;
        self.next_task_id = self.next_task_id.wrapping_add(1);
        self.active_task_id = Some(task_id);
        Some(task_id)
    }

    fn finish(&mut self, task_id: u64) -> bool {
        if self.active_task_id != Some(task_id) {
            return false;
        }
        self.active_task_id = None;
        true
    }

    fn is_active(&self) -> bool {
        self.active_task_id.is_some()
    }
}

struct SupervisorVerdictTaskResult {
    agent_id: AgentId,
    activity_counter: u64,
    baseline: SupervisionBaseline,
    attempts_started: u8,
    verdict_settings: VerdictSettingsFingerprint,
    result: Result<
        crate::agent::supervisor::SupervisionVerdict,
        crate::agent::supervisor::SupervisionFailure,
    >,
}

struct SupervisorVerdictTaskEvent {
    task_id: u64,
    result: SupervisorVerdictTaskResult,
}

struct SupervisorVerdictTaskCompletion {
    task_id: u64,
    tx: mpsc::UnboundedSender<SupervisorVerdictTaskEvent>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    aborted: Option<SupervisorVerdictTaskResult>,
}

impl SupervisorVerdictTaskCompletion {
    fn complete(mut self, result: SupervisorVerdictTaskResult) {
        self.permit.take();
        self.aborted = None;
        let _ = self.tx.send(SupervisorVerdictTaskEvent {
            task_id: self.task_id,
            result,
        });
    }
}

impl Drop for SupervisorVerdictTaskCompletion {
    fn drop(&mut self) {
        self.permit.take();
        let Some(result) = self.aborted.take() else {
            return;
        };
        let _ = self.tx.send(SupervisorVerdictTaskEvent {
            task_id: self.task_id,
            result,
        });
    }
}

enum SupervisorCompactionTaskEvent {
    Started {
        agent_id: AgentId,
        activity_counter: u64,
        accepted: bool,
    },
    Finished {
        agent_id: AgentId,
        activity_counter: u64,
    },
}

#[cfg(test)]
struct SupervisorVerdictPostSampleTestGate {
    agent_id: AgentId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static SUPERVISOR_VERDICT_POST_SAMPLE_TEST_GATE: StdMutex<
    Option<SupervisorVerdictPostSampleTestGate>,
> = StdMutex::new(None);

#[cfg(test)]
fn install_supervisor_verdict_post_sample_test_gate(
    agent_id: AgentId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let replaced = SUPERVISOR_VERDICT_POST_SAMPLE_TEST_GATE
        .lock()
        .expect("supervisor verdict test gate mutex poisoned")
        .replace(SupervisorVerdictPostSampleTestGate {
            agent_id,
            entered: entered_tx,
            release: release_rx,
        });
    assert!(
        replaced.is_none(),
        "supervisor verdict test gate already installed"
    );
    (entered_rx, release_tx)
}

#[cfg(test)]
async fn wait_for_supervisor_verdict_post_sample_test_gate(agent_id: &AgentId) {
    let gate = {
        let mut slot = SUPERVISOR_VERDICT_POST_SAMPLE_TEST_GATE
            .lock()
            .expect("supervisor verdict test gate mutex poisoned");
        if slot.as_ref().is_some_and(|gate| &gate.agent_id == agent_id) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        let _ = gate.entered.send(());
        let _ = gate.release.await;
    }
}

#[cfg(not(test))]
async fn wait_for_supervisor_verdict_post_sample_test_gate(_agent_id: &AgentId) {}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisorVerdictCallStart {
    agent_id: AgentId,
    activity_counter: u64,
    attempts_started: u8,
    cost_hint: Option<protocol::SpawnCostHint>,
}

#[cfg(test)]
struct SupervisorVerdictCallTestGate {
    agent_id: AgentId,
    started: mpsc::UnboundedSender<SupervisorVerdictCallStart>,
    releases: mpsc::UnboundedReceiver<()>,
}

#[cfg(test)]
fn supervisor_verdict_call_test_gates()
-> &'static StdMutex<HashMap<AgentId, SupervisorVerdictCallTestGate>> {
    static GATES: std::sync::OnceLock<StdMutex<HashMap<AgentId, SupervisorVerdictCallTestGate>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(|| StdMutex::new(HashMap::new()))
}

#[cfg(test)]
fn install_supervisor_verdict_call_test_gate(
    agent_id: AgentId,
) -> (
    mpsc::UnboundedReceiver<SupervisorVerdictCallStart>,
    mpsc::UnboundedSender<()>,
) {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = mpsc::unbounded_channel();
    let replaced = supervisor_verdict_call_test_gates()
        .lock()
        .expect("supervisor verdict call test gate mutex poisoned")
        .insert(
            agent_id.clone(),
            SupervisorVerdictCallTestGate {
                agent_id,
                started: started_tx,
                releases: release_rx,
            },
        );
    assert!(replaced.is_none(), "call test gate already installed");
    (started_rx, release_tx)
}

#[cfg(test)]
fn remove_supervisor_verdict_call_test_gate(agent_id: &AgentId) {
    let gate = supervisor_verdict_call_test_gates()
        .lock()
        .expect("supervisor verdict call test gate mutex poisoned")
        .remove(agent_id);
    assert!(
        gate.as_ref().is_some_and(|gate| &gate.agent_id == agent_id),
        "call test gate belonged to another agent"
    );
}

#[cfg(test)]
async fn wait_for_supervisor_verdict_call_test_gate(
    agent_id: &AgentId,
    activity_counter: u64,
    attempts_started: u8,
    cost_hint: Option<protocol::SpawnCostHint>,
) {
    let gate = {
        supervisor_verdict_call_test_gates()
            .lock()
            .expect("supervisor verdict call test gate mutex poisoned")
            .remove(agent_id)
    };
    let Some(mut gate) = gate else {
        return;
    };
    let _ = gate.started.send(SupervisorVerdictCallStart {
        agent_id: agent_id.clone(),
        activity_counter,
        attempts_started,
        cost_hint,
    });
    let _ = gate.releases.recv().await;
    supervisor_verdict_call_test_gates()
        .lock()
        .expect("supervisor verdict call test gate mutex poisoned")
        .insert(agent_id.clone(), gate);
}

#[cfg(not(test))]
async fn wait_for_supervisor_verdict_call_test_gate(
    _agent_id: &AgentId,
    _activity_counter: u64,
    _attempts_started: u8,
    _cost_hint: Option<protocol::SpawnCostHint>,
) {
}

#[derive(Clone)]
struct ActivitySummaryObservation {
    agent_id: AgentId,
    handle: AgentHandle,
    start: AgentStartPayload,
    status: crate::agent::registry::AgentStatus,
}

#[derive(Default)]
struct ActivitySummarySchedulerEntry {
    last_activity_counter: u64,
    first_meaningful_at: Option<Instant>,
    pending_due: Option<Instant>,
    queued_final_refresh: bool,
    in_flight: bool,
    was_active: bool,
    last_call_at: Option<Instant>,
    backoff_until: Option<Instant>,
    last_summarized_through_seq: Option<u64>,
    latest_observed_through_seq: Option<u64>,
}

#[derive(Debug)]
struct ActivitySummaryTaskStarted {
    agent_id: AgentId,
    epoch: u64,
    requested_at_ms: u64,
    previous_summary: Option<AgentActivitySummary>,
}

#[derive(Debug)]
struct ActivitySummaryTaskResult {
    agent_id: AgentId,
    epoch: u64,
    transient_agent_id: AgentId,
    source_through_seq: Option<u64>,
    result: Result<AgentActivitySummary, String>,
}

#[derive(Debug)]
enum ActivitySummaryTaskEvent {
    Started(ActivitySummaryTaskStarted),
    Finished(ActivitySummaryTaskResult),
}

#[derive(Clone, Debug)]
pub(crate) struct TeamMemberMessageOutcome {
    pub member_id: TeamMemberId,
    pub agent_id: AgentId,
    pub queued: bool,
}

#[derive(Clone, Debug)]
struct TeamCompactTarget {
    member_id: TeamMemberId,
    agent_id: AgentId,
}

struct TeamAgentCompaction {
    target: TeamCompactTarget,
    compaction: AgentCompaction,
    rx: mpsc::UnboundedReceiver<protocol::Envelope>,
}

struct AgentCompaction {
    agent_id: AgentId,
    agent_handle: AgentHandle,
    start: AgentStartPayload,
    old_session_id: SessionId,
    access_mode: protocol::BackendAccessMode,
    old_record: SessionRecord,
    session_store: Arc<Mutex<SessionStore>>,
    team_registry: TeamRegistryHandle,
    stream: Stream,
    summary_rx: oneshot::Receiver<Result<CompactionSummary, String>>,
}

#[derive(Clone, Debug)]
enum KiroSessionSchemaState {
    Pending,
    Ready(SessionSettingsSchema),
    Unavailable(String),
}

#[derive(Clone, Debug)]
enum CodexSessionSchemaState {
    Pending,
    Ready(SessionSettingsSchema),
    Unavailable(String),
}

#[derive(Clone, Debug)]
enum HermesSessionSchemaState {
    Pending,
    Ready(SessionSettingsSchema),
    Unavailable(String),
}

enum SessionSchemaResolution {
    Pending,
    Ready(SessionSettingsSchema),
    Unavailable(String),
}

pub(crate) struct HostState {
    pub registry: AgentRegistry,
    pub review_registry: ReviewRegistryHandle,
    pub team_registry: TeamRegistryHandle,
    pub project_store: Arc<Mutex<ProjectStore>>,
    pub settings_store: Arc<Mutex<HostSettingsStore>>,
    pub agents_view_preferences_store: Option<Arc<Mutex<AgentsViewPreferencesStore>>>,
    pub session_store: Arc<Mutex<SessionStore>>,
    pub custom_agent_store: Arc<Mutex<CustomAgentStore>>,
    pub mcp_server_store: Arc<Mutex<McpServerStore>>,
    pub steering_store: Arc<Mutex<SteeringStore>>,
    pub skill_store: Arc<Mutex<SkillStore>>,
    pub agent_sessions: HashMap<AgentId, SessionId>,
    pending_agent_sessions: HashMap<AgentId, SessionId>,
    agent_visibility: AgentVisibilityRegistry,
    pub agent_activity_summaries: HashMap<AgentId, AgentActivitySummaryState>,
    closed_agent_usage_snapshots: HashMap<AgentId, AgentUsageSnapshot>,
    activity_summary_epoch: u64,
    activity_summary_settings_tx: watch::Sender<ActivitySummarySettingsSignal>,
    supervisor_epoch: u64,
    supervisor_settings_tx: watch::Sender<SupervisorSettingsSignal>,
    pub sub_agent_spawn_tx: HostSubAgentSpawnTx,
    pub capacity_tx: HostCapacityTx,
    pub use_mock_backend: bool,
    pub debug_mcp: DebugMcpHandle,
    pub agent_control_mcp: AgentControlMcpHandle,
    pub config_mcp: ConfigMcpHandle,
    pub review_mcp: ReviewMcpHandle,
    pub workflow_mcp: WorkflowMcpHandle,
    pub workflow_watcher: WorkflowWatcherHandle,
    pub workflow_catalog: WorkflowCatalog,
    pub workflow_locations: Vec<WorkflowCatalogLocation>,
    pub workflow_run_store: WorkflowRunStore,
    pub mobile_access: MobileAccessHandle,
    codex_session_schema: CodexSessionSchemaState,
    kiro_session_schema: KiroSessionSchemaState,
    hermes_session_schema: HermesSessionSchemaState,
    /// Hermes profiles discovered by the last session-schema probe; drives
    /// the synthesized "Hermes — <profile>" launch-profile entries.
    hermes_launch_profiles: Vec<crate::backend::hermes::HermesLaunchProfileInfo>,
    backend_config_snapshots: Vec<BackendConfigSnapshot>,
    backend_native_settings_snapshots: Vec<BackendNativeSettingsSnapshot>,
    backend_capacity: HashMap<BackendKind, BackendCapacitySnapshot>,
    antigravity_conversations_dir: PathBuf,
    codex_probe_program: Option<String>,
    kiro_probe_program: Option<String>,
    kiro_probe_workspace_root: Option<PathBuf>,
    skip_real_backend_probe: bool,
    host_streams: HashMap<StreamPath, HostSubscriber>,
    project_streams: HashMap<ProjectId, ProjectStreamSubscription>,
    terminal_streams: HashMap<(StreamPath, TerminalId), TerminalHandle>,
    browse_streams: HashMap<(StreamPath, StreamPath), Stream>,
    workbench_parent_locks: HashMap<ProjectId, Weak<Mutex<()>>>,
    /// Per-project "active search id". Each project-wide search stores its
    /// `search_id` here before spawning its walk; the walk polls this atomic
    /// and aborts as soon as the value no longer matches (a superseding search
    /// or an explicit cancel changed it).
    project_search_ids: HashMap<ProjectId, Arc<AtomicU64>>,
    /// Per-project thin code-intelligence router. Maps each `CodeIntel*` frame
    /// to the owning root's `CodeIntelService` actor (lazily spawned). Holds no
    /// provider state itself. See `server/src/code_intel/`.
    code_intel_routers: HashMap<ProjectId, CodeIntelRouter>,
    /// Workbench projects currently being removed. Removal validates
    /// blockers against a snapshot and then runs slow git subprocesses
    /// outside the state lock; agent spawns and terminal creation check
    /// this set so they cannot race into a half-removed workbench.
    removing_projects: HashSet<ProjectId>,
    #[cfg(feature = "test-support")]
    agent_name_test_gate: Option<Arc<AgentNameTestGateInner>>,
    #[cfg(feature = "test-support")]
    session_schema_probe_count: u64,
}

impl Drop for HostState {
    fn drop(&mut self) {
        for router in self.code_intel_routers.values_mut() {
            router.shutdown_all();
        }
        self.mobile_access.shutdown();
    }
}

pub struct HostHandle {
    state: Arc<Mutex<HostState>>,
    workflow_save_lock: Arc<Mutex<()>>,
    backend_setup_refresh_lock: Arc<Mutex<()>>,
    session_schema_refresh_lock: Arc<Mutex<()>>,
    spawn_operations: SpawnOperationHandle,
    spawn_operation_owner: Option<Arc<SpawnOperationOwner>>,
}

const SPAWN_OPERATION_QUEUE_CAPACITY: usize = 32;
const MAX_CONCURRENT_SPAWN_OPERATIONS: usize = 8;

struct SpawnOperationOwner {
    cancel: CancellationToken,
    worker: StdMutex<Option<SpawnOperationWorker>>,
    shutdown: StdMutex<SpawnOperationShutdown>,
    shutdown_complete: tokio::sync::watch::Sender<bool>,
    #[cfg(feature = "test-support")]
    completion_test_gate: Arc<StdMutex<Option<Arc<SpawnOperationTestGateInner>>>>,
    #[cfg(feature = "test-support")]
    start_test_gate: Arc<StdMutex<Option<Arc<SpawnOperationTestGateInner>>>>,
    #[cfg(feature = "test-support")]
    drain_test_gate: Arc<StdMutex<Option<Arc<SpawnOperationTestGateInner>>>>,
    #[cfg(feature = "test-support")]
    publication_test_gate: Arc<StdMutex<Option<Arc<SpawnOperationTestGateInner>>>>,
}

struct SpawnOperationShutdown {
    started: bool,
}

enum SpawnOperationWorker {
    Tokio(JoinHandle<()>),
    Thread(std::thread::JoinHandle<()>),
}

impl Drop for SpawnOperationOwner {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

impl SpawnOperationOwner {
    fn begin_shutdown(&self) {
        let mut shutdown = self
            .shutdown
            .lock()
            .expect("spawn operation shutdown mutex poisoned");
        if shutdown.started {
            return;
        }
        shutdown.started = true;
        self.cancel.cancel();
        let worker = self
            .worker
            .lock()
            .expect("spawn operation worker mutex poisoned")
            .take();
        let shutdown_complete = self.shutdown_complete.clone();
        match worker {
            Some(SpawnOperationWorker::Tokio(worker)) => {
                std::thread::Builder::new()
                    .name("tyde-host-spawn-operation-drain".to_owned())
                    .spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("failed to build host spawn operation drain runtime");
                        let _ = runtime.block_on(worker);
                        shutdown_complete.send_replace(true);
                    })
                    .expect("failed to spawn host spawn operation drain thread");
            }
            Some(SpawnOperationWorker::Thread(worker)) => {
                std::thread::Builder::new()
                    .name("tyde-host-spawn-operation-drain".to_owned())
                    .spawn(move || {
                        let _ = worker.join();
                        shutdown_complete.send_replace(true);
                    })
                    .expect("failed to spawn host spawn operation drain thread");
            }
            None => {
                shutdown_complete.send_replace(true);
            }
        }
    }

    async fn shutdown(&self) {
        let mut shutdown_complete = self.shutdown_complete.subscribe();
        self.begin_shutdown();
        while !*shutdown_complete.borrow_and_update() {
            if shutdown_complete.changed().await.is_err() {
                break;
            }
        }
    }
}

#[derive(Clone)]
struct SpawnOperationHandle {
    tx: mpsc::Sender<SpawnOperation>,
    owner: Weak<SpawnOperationOwner>,
}

struct SpawnOperation {
    payload: SpawnAgentPayload,
    request_stream: StreamPath,
    output_stream: Stream,
}

#[derive(Clone)]
struct SpawnOperationTerminal {
    request_stream: StreamPath,
    output_stream: Stream,
}

struct ActiveSpawnOperation {
    terminal: SpawnOperationTerminal,
    outcome: Arc<StdMutex<Option<SpawnOperationOutcome>>>,
}

enum SpawnOperationOutcome {
    Success,
    Error(AppError),
    Panicked,
}

#[derive(Clone)]
struct SpawnOperationTerminalClaim {
    outcome: Arc<StdMutex<Option<SpawnOperationOutcome>>>,
    #[cfg(feature = "test-support")]
    publication_test_gate: Arc<StdMutex<Option<Arc<SpawnOperationTestGateInner>>>>,
}

impl SpawnOperationTerminalClaim {
    fn claim_success_at_publication(&self) -> bool {
        let mut outcome = self
            .outcome
            .lock()
            .expect("spawn operation outcome mutex poisoned");
        if outcome.is_some() {
            false
        } else {
            *outcome = Some(SpawnOperationOutcome::Success);
            true
        }
    }

    async fn wait_after_success_publication(&self, claimed: bool) {
        #[cfg(feature = "test-support")]
        if claimed {
            wait_for_spawn_operation_test_gate(&self.publication_test_gate).await;
        }
        #[cfg(not(feature = "test-support"))]
        let _ = claimed;
    }

    fn claim_resolved_result(
        &self,
        result: Result<AppResult<AgentId>, Box<dyn std::any::Any + Send>>,
    ) {
        let mut outcome = self
            .outcome
            .lock()
            .expect("spawn operation outcome mutex poisoned");
        if outcome.is_some() {
            return;
        }
        *outcome = Some(match result {
            Ok(Ok(_)) => SpawnOperationOutcome::Success,
            Ok(Err(error)) => SpawnOperationOutcome::Error(error),
            Err(_) => SpawnOperationOutcome::Panicked,
        });
    }
}

struct WeakHostHandle {
    state: Weak<Mutex<HostState>>,
    workflow_save_lock: Weak<Mutex<()>>,
    backend_setup_refresh_lock: Weak<Mutex<()>>,
    session_schema_refresh_lock: Weak<Mutex<()>>,
    spawn_operations: SpawnOperationHandle,
}

impl WeakHostHandle {
    fn upgrade(&self) -> Option<HostHandle> {
        Some(HostHandle {
            state: self.state.upgrade()?,
            workflow_save_lock: self.workflow_save_lock.upgrade()?,
            backend_setup_refresh_lock: self.backend_setup_refresh_lock.upgrade()?,
            session_schema_refresh_lock: self.session_schema_refresh_lock.upgrade()?,
            spawn_operations: self.spawn_operations.clone(),
            spawn_operation_owner: None,
        })
    }
}

impl Clone for HostHandle {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            workflow_save_lock: Arc::clone(&self.workflow_save_lock),
            backend_setup_refresh_lock: Arc::clone(&self.backend_setup_refresh_lock),
            session_schema_refresh_lock: Arc::clone(&self.session_schema_refresh_lock),
            spawn_operations: self.spawn_operations.clone(),
            spawn_operation_owner: None,
        }
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        if let Some(owner) = self.spawn_operation_owner.as_ref() {
            owner.begin_shutdown();
        }
    }
}

struct PendingAgentSessionPublication {
    agent_id: AgentId,
    publish_tx: Option<mpsc::UnboundedSender<()>>,
}

#[derive(Clone, Default)]
struct AgentVisibilityRegistry {
    inner: Arc<std::sync::Mutex<HashMap<AgentId, AgentVisibilityEntry>>>,
}

struct AgentVisibilityEntry {
    phase: AgentVisibilityPhase,
    fanout_active: bool,
    host_streams: HashSet<StreamPath>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentVisibilityPhase {
    Pending,
    Fanout,
    Visible,
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupFailureVisibility {
    CleanupUnpublished,
    AwaitPublication,
}

impl Default for AgentVisibilityEntry {
    fn default() -> Self {
        Self {
            phase: AgentVisibilityPhase::Visible,
            fanout_active: false,
            host_streams: HashSet::new(),
        }
    }
}

impl AgentVisibilityRegistry {
    fn begin_pending(&self, agent_id: AgentId) {
        self.inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .insert(
                agent_id,
                AgentVisibilityEntry {
                    phase: AgentVisibilityPhase::Pending,
                    fanout_active: false,
                    host_streams: HashSet::new(),
                },
            );
    }

    fn begin_fanout(&self, agent_id: &AgentId) -> bool {
        let mut visibility = self.inner.lock().expect("agent visibility mutex poisoned");
        let Some(entry) = visibility.get_mut(agent_id) else {
            return false;
        };
        if entry.phase != AgentVisibilityPhase::Pending {
            return false;
        }
        entry.phase = AgentVisibilityPhase::Fanout;
        entry.fanout_active = true;
        true
    }

    fn claim_startup_failure(&self, agent_id: &AgentId) -> StartupFailureVisibility {
        let mut visibility = self.inner.lock().expect("agent visibility mutex poisoned");
        let Some(entry) = visibility.get_mut(agent_id) else {
            return StartupFailureVisibility::CleanupUnpublished;
        };
        match entry.phase {
            AgentVisibilityPhase::Pending => {
                entry.phase = AgentVisibilityPhase::Cancelled;
                entry.fanout_active = false;
                StartupFailureVisibility::CleanupUnpublished
            }
            AgentVisibilityPhase::Fanout | AgentVisibilityPhase::Visible => {
                StartupFailureVisibility::AwaitPublication
            }
            AgentVisibilityPhase::Cancelled => StartupFailureVisibility::CleanupUnpublished,
        }
    }

    fn may_emit_new_agent(&self, agent_id: &AgentId) -> bool {
        self.inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .get(agent_id)
            .is_some_and(|entry| entry.phase == AgentVisibilityPhase::Fanout && entry.fanout_active)
    }

    fn record_new_agent_during_fanout(&self, agent_id: AgentId, host_stream: StreamPath) -> bool {
        let mut visibility = self.inner.lock().expect("agent visibility mutex poisoned");
        let entry = visibility.entry(agent_id).or_default();
        entry.host_streams.insert(host_stream);
        entry.phase == AgentVisibilityPhase::Fanout && entry.fanout_active
    }

    fn finish_fanout(&self, agent_id: &AgentId) -> bool {
        let mut visibility = self.inner.lock().expect("agent visibility mutex poisoned");
        let Some(entry) = visibility.get_mut(agent_id) else {
            return true;
        };
        entry.fanout_active = false;
        if entry.phase == AgentVisibilityPhase::Fanout {
            entry.phase = AgentVisibilityPhase::Visible;
            false
        } else {
            true
        }
    }

    fn cancel_outer_spawn(&self, agent_id: &AgentId) {
        if let Some(entry) = self
            .inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .get_mut(agent_id)
        {
            entry.phase = AgentVisibilityPhase::Cancelled;
            entry.fanout_active = false;
        }
    }

    fn request_cleanup(&self, agent_id: &AgentId) -> bool {
        let mut visibility = self.inner.lock().expect("agent visibility mutex poisoned");
        let Some(entry) = visibility.get_mut(agent_id) else {
            return false;
        };
        entry.phase = AgentVisibilityPhase::Cancelled;
        entry.fanout_active
    }

    fn cleanup_is_requested(&self, agent_id: &AgentId) -> bool {
        self.inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .get(agent_id)
            .is_some_and(|entry| entry.phase == AgentVisibilityPhase::Cancelled)
    }

    fn bootstrap_eligible(&self, agent_id: &AgentId, publicly_bound: bool) -> bool {
        match self
            .inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .get(agent_id)
        {
            Some(entry) => entry.phase == AgentVisibilityPhase::Visible,
            None => publicly_bound,
        }
    }

    fn record_new_agent(&self, agent_id: AgentId, host_stream: StreamPath) {
        self.inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .entry(agent_id)
            .or_default()
            .host_streams
            .insert(host_stream);
    }

    fn visible_host_streams(&self, agent_id: &AgentId) -> HashSet<StreamPath> {
        self.inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .get(agent_id)
            .map(|entry| entry.host_streams.clone())
            .unwrap_or_default()
    }

    fn remove_agent(&self, agent_id: &AgentId) {
        self.inner
            .lock()
            .expect("agent visibility mutex poisoned")
            .remove(agent_id);
    }

    fn remove_host_stream(&self, host_stream: &StreamPath) {
        let mut visibility = self.inner.lock().expect("agent visibility mutex poisoned");
        for entry in visibility.values_mut() {
            entry.host_streams.remove(host_stream);
        }
    }
}

#[derive(Clone)]
struct SpawnVisibility {
    changed: Arc<tokio::sync::Notify>,
    agent_id: AgentId,
    agent_visibility: AgentVisibilityRegistry,
}

struct SpawnVisibilityGuard {
    visibility: SpawnVisibility,
    armed: bool,
}

impl SpawnVisibility {
    fn new(agent_id: AgentId, agent_visibility: AgentVisibilityRegistry) -> Self {
        agent_visibility.begin_pending(agent_id.clone());
        Self {
            changed: Arc::new(tokio::sync::Notify::new()),
            agent_id,
            agent_visibility,
        }
    }

    fn begin_fanout(&self) -> bool {
        self.agent_visibility.begin_fanout(&self.agent_id)
    }

    fn may_emit_new_agent(&self) -> bool {
        self.agent_visibility.may_emit_new_agent(&self.agent_id)
    }

    /// Records every successful send, even if cancellation raced after the
    /// send began. That is the exact set which is entitled to AgentClosed.
    fn record_new_agent_delivery(&self, host_stream: StreamPath) -> bool {
        self.agent_visibility
            .record_new_agent_during_fanout(self.agent_id.clone(), host_stream)
    }

    /// Returns true when outer cancellation asked the fanout to stop.
    fn finish_new_agent_fanout(&self) -> bool {
        let cleanup_requested = self.agent_visibility.finish_fanout(&self.agent_id);
        self.changed.notify_one();
        cleanup_requested
    }

    fn cancel_outer_spawn(&self) {
        self.agent_visibility.cancel_outer_spawn(&self.agent_id);
        self.changed.notify_one();
    }

    fn cleanup_is_requested(&self) -> bool {
        self.agent_visibility.cleanup_is_requested(&self.agent_id)
    }

    async fn request_cleanup(&self) {
        loop {
            let wait_for_fanout = self.agent_visibility.request_cleanup(&self.agent_id);
            if !wait_for_fanout {
                return;
            }
            self.changed.notified().await;
        }
    }

    fn claim_startup_failure(&self) -> StartupFailureVisibility {
        let outcome = self.agent_visibility.claim_startup_failure(&self.agent_id);
        if outcome == StartupFailureVisibility::CleanupUnpublished {
            self.changed.notify_one();
        }
        outcome
    }
}

impl SpawnVisibilityGuard {
    fn new(visibility: SpawnVisibility) -> Self {
        Self {
            visibility,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SpawnVisibilityGuard {
    fn drop(&mut self) {
        if self.armed {
            self.visibility.cancel_outer_spawn();
        }
    }
}

impl PendingAgentSessionPublication {
    fn publish(mut self) {
        let Some(publish_tx) = self.publish_tx.take() else {
            tracing::error!(
                agent_id = %self.agent_id,
                "agent session publication guard was consumed before publication"
            );
            return;
        };
        if publish_tx.send(()).is_err() {
            tracing::debug!(
                agent_id = %self.agent_id,
                "agent session registration ended before publication gate"
            );
        }
    }
}

impl Drop for PendingAgentSessionPublication {
    fn drop(&mut self) {
        self.publish_tx.take();
    }
}

#[cfg(feature = "test-support")]
pub struct InstalledWorkbenchRemoveHook {
    inner: Arc<WorkbenchRemoveHook>,
}

#[cfg(feature = "test-support")]
pub struct InstalledAgentNameGate {
    inner: Arc<AgentNameTestGateInner>,
}

#[cfg(feature = "test-support")]
pub struct InstalledSpawnOperationTestGate {
    inner: Arc<SpawnOperationTestGateInner>,
}

#[cfg(feature = "test-support")]
pub(crate) struct SpawnOperationTestGateInner {
    entered_tx: mpsc::UnboundedSender<()>,
    entered_rx: Mutex<mpsc::UnboundedReceiver<()>>,
    release: Semaphore,
    panic_on_release: AtomicBool,
}

#[cfg(feature = "test-support")]
impl InstalledSpawnOperationTestGate {
    pub async fn wait_until_entered(&self) {
        self.inner
            .entered_rx
            .lock()
            .await
            .recv()
            .await
            .expect("spawn operation test gate closed before entry");
    }

    pub fn release_one(&self) {
        self.inner.release.add_permits(1);
    }

    pub fn panic_on_release(&self) {
        self.inner.panic_on_release.store(true, Ordering::SeqCst);
    }
}

#[cfg(feature = "test-support")]
impl Drop for InstalledSpawnOperationTestGate {
    fn drop(&mut self) {
        self.inner.release.close();
    }
}

#[cfg(feature = "test-support")]
struct AgentNameTestGateInner {
    next_ordinal: AtomicU64,
    entered_tx: mpsc::UnboundedSender<u64>,
    entered_rx: Mutex<mpsc::UnboundedReceiver<u64>>,
    completed_tx: mpsc::UnboundedSender<u64>,
    completed_rx: Mutex<mpsc::UnboundedReceiver<u64>>,
    release: Semaphore,
    panic_ordinals: StdMutex<HashSet<u64>>,
}

#[cfg(feature = "test-support")]
struct AgentNameTestCompletion {
    gate: Arc<AgentNameTestGateInner>,
    ordinal: u64,
}

#[cfg(not(feature = "test-support"))]
type AgentNameTestCompletion = ();

#[cfg(feature = "test-support")]
impl InstalledAgentNameGate {
    pub async fn wait_until_entered(&self) -> u64 {
        self.inner
            .entered_rx
            .lock()
            .await
            .recv()
            .await
            .expect("agent name gate closed before entry")
    }

    pub async fn wait_until_completed(&self) -> u64 {
        self.inner
            .completed_rx
            .lock()
            .await
            .recv()
            .await
            .expect("agent name gate closed before completion")
    }

    pub fn try_next_entry(&self) -> Option<u64> {
        self.inner.entered_rx.try_lock().ok()?.try_recv().ok()
    }

    pub fn release_one(&self) {
        self.inner.release.add_permits(1);
    }

    pub fn panic_on_release(&self, ordinal: u64) {
        self.inner
            .panic_ordinals
            .lock()
            .expect("agent name gate panic set poisoned")
            .insert(ordinal);
    }
}

#[cfg(feature = "test-support")]
impl Drop for InstalledAgentNameGate {
    fn drop(&mut self) {
        self.inner.release.close();
    }
}

#[cfg(feature = "test-support")]
struct WorkbenchRemoveHook {
    host_state_ptr: usize,
    reached: tokio::sync::Notify,
    spawn_waiting: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(feature = "test-support")]
impl InstalledWorkbenchRemoveHook {
    pub async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }

    pub fn resume(&self) {
        self.inner.resume.notify_one();
    }

    pub async fn wait_until_spawn_waiting(&self) {
        self.inner.spawn_waiting.notified().await;
    }
}

#[cfg(feature = "test-support")]
impl Drop for InstalledWorkbenchRemoveHook {
    fn drop(&mut self) {
        let mut hook = workbench_remove_hook_cell()
            .lock()
            .expect("workbench remove hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(feature = "test-support")]
impl HostHandle {
    /// Drives the same passive adapter ingress used by a live agent, while
    /// keeping the test fixture deterministic and host-scoped.
    pub async fn ingest_passive_adapter_notification_for_test(
        &self,
        backend_kind: BackendKind,
        payload: serde_json::Value,
    ) -> bool {
        let emitter = {
            let state = self.state.lock().await;
            HostSubAgentEmitter::new(
                state.sub_agent_spawn_tx.clone(),
                state.capacity_tx.clone(),
                AgentId("passive-capacity-test-agent".to_owned()),
                Vec::new(),
            )
        };
        let forwarded = match backend_kind {
            BackendKind::Claude => {
                crate::backend::claude::forward_passive_rate_limit_event(&payload, &emitter)
            }
            BackendKind::Codex => {
                crate::backend::codex::forward_passive_rate_limits_updated(&payload, &emitter);
                true
            }
            _ => false,
        };
        if !forwarded {
            return false;
        }
        let (barrier_tx, barrier_rx) = oneshot::channel();
        let capacity_tx = self.state.lock().await.capacity_tx.clone();
        if capacity_tx
            .send(HostCapacityUpdate::Barrier(barrier_tx))
            .is_err()
        {
            return false;
        }
        barrier_rx.await.is_ok()
    }

    #[cfg(feature = "test-support")]
    pub async fn mark_backend_capacity_stale_for_test(&self, backend_kind: BackendKind) {
        let retrieved_at_ms = self
            .state
            .lock()
            .await
            .backend_capacity
            .get(&backend_kind)
            .map(|snapshot| snapshot.retrieved_at_ms);
        if let Some(retrieved_at_ms) = retrieved_at_ms {
            self.mark_backend_capacity_stale(backend_kind, retrieved_at_ms)
                .await;
        }
    }

    pub async fn age_backend_capacity_for_test(&self, backend_kind: BackendKind, age_ms: u64) {
        let mut state = self.state.lock().await;
        let now = capacity_now_ms();
        let snapshot = state
            .backend_capacity
            .get_mut(&backend_kind)
            .expect("capacity snapshot must exist before aging it");
        snapshot.retrieved_at_ms = now.saturating_sub(age_ms);
    }

    pub fn install_workbench_remove_test_hook(&self) -> InstalledWorkbenchRemoveHook {
        let inner = Arc::new(WorkbenchRemoveHook {
            host_state_ptr: Arc::as_ptr(&self.state) as usize,
            reached: tokio::sync::Notify::new(),
            spawn_waiting: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let mut hook = workbench_remove_hook_cell()
            .lock()
            .expect("workbench remove hook mutex poisoned");
        assert!(hook.is_none(), "workbench remove hook already installed");
        *hook = Some(Arc::clone(&inner));
        InstalledWorkbenchRemoveHook { inner }
    }

    pub async fn install_agent_name_test_gate(&self) -> InstalledAgentNameGate {
        let (entered_tx, entered_rx) = mpsc::unbounded_channel();
        let (completed_tx, completed_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(AgentNameTestGateInner {
            next_ordinal: AtomicU64::new(0),
            entered_tx,
            entered_rx: Mutex::new(entered_rx),
            completed_tx,
            completed_rx: Mutex::new(completed_rx),
            release: Semaphore::new(0),
            panic_ordinals: StdMutex::new(HashSet::new()),
        });
        self.state.lock().await.agent_name_test_gate = Some(Arc::clone(&inner));
        InstalledAgentNameGate { inner }
    }

    pub async fn set_session_schema_ready_for_test(&self, backend_kind: BackendKind) {
        let schema = session_settings_schema_for_backend(backend_kind);
        let _refresh_guard = self.session_schema_refresh_lock.lock().await;
        let mut state = self.state.lock().await;
        match backend_kind {
            BackendKind::Codex => {
                state.codex_session_schema = CodexSessionSchemaState::Ready(schema)
            }
            BackendKind::Kiro => state.kiro_session_schema = KiroSessionSchemaState::Ready(schema),
            BackendKind::Hermes => {
                state.hermes_session_schema = HermesSessionSchemaState::Ready(schema)
            }
            _ => panic!("backend {backend_kind:?} does not have a dynamic session schema"),
        }
        if matches!(
            &state.codex_session_schema,
            CodexSessionSchemaState::Pending
        ) {
            state.codex_session_schema =
                CodexSessionSchemaState::Unavailable("test schema not configured".to_owned());
        }
        if matches!(&state.kiro_session_schema, KiroSessionSchemaState::Pending) {
            state.kiro_session_schema =
                KiroSessionSchemaState::Unavailable("test schema not configured".to_owned());
        }
        if matches!(
            &state.hermes_session_schema,
            HermesSessionSchemaState::Pending
        ) {
            state.hermes_session_schema =
                HermesSessionSchemaState::Unavailable("test schema not configured".to_owned());
        }
    }

    pub async fn set_session_schema_unavailable_for_test(
        &self,
        backend_kind: BackendKind,
        message: &str,
    ) {
        let _refresh_guard = self.session_schema_refresh_lock.lock().await;
        let mut state = self.state.lock().await;
        match backend_kind {
            BackendKind::Codex => {
                state.codex_session_schema =
                    CodexSessionSchemaState::Unavailable(message.to_owned())
            }
            BackendKind::Kiro => {
                state.kiro_session_schema = KiroSessionSchemaState::Unavailable(message.to_owned())
            }
            BackendKind::Hermes => {
                state.hermes_session_schema =
                    HermesSessionSchemaState::Unavailable(message.to_owned())
            }
            _ => panic!("backend {backend_kind:?} does not have a dynamic session schema"),
        }
    }

    pub async fn session_schema_probe_count_for_test(&self) -> u64 {
        self.state.lock().await.session_schema_probe_count
    }

    pub fn install_spawn_operation_completion_test_gate(&self) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        let owner = self
            .spawn_operations
            .owner
            .upgrade()
            .expect("spawn operation owner must be available while installing test gate");
        *owner
            .completion_test_gate
            .lock()
            .expect("spawn operation completion test gate mutex poisoned") =
            Some(Arc::clone(&gate.inner));
        gate
    }

    pub fn install_spawn_operation_start_test_gate(&self) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        let owner = self
            .spawn_operations
            .owner
            .upgrade()
            .expect("spawn operation owner must be available while installing test gate");
        *owner
            .start_test_gate
            .lock()
            .expect("spawn operation start test gate mutex poisoned") =
            Some(Arc::clone(&gate.inner));
        gate
    }

    pub fn install_spawn_operation_drain_test_gate(&self) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        let owner = self
            .spawn_operations
            .owner
            .upgrade()
            .expect("spawn operation owner must be available while installing test gate");
        *owner
            .drain_test_gate
            .lock()
            .expect("spawn operation drain test gate mutex poisoned") =
            Some(Arc::clone(&gate.inner));
        gate
    }

    pub fn install_spawn_operation_publication_test_gate(&self) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        let owner = self
            .spawn_operations
            .owner
            .upgrade()
            .expect("spawn operation owner must be available while installing test gate");
        *owner
            .publication_test_gate
            .lock()
            .expect("spawn operation publication test gate mutex poisoned") =
            Some(Arc::clone(&gate.inner));
        gate
    }

    pub fn install_agent_startup_completion_test_gate(
        &self,
        agent_name: &str,
    ) -> InstalledSpawnOperationTestGate {
        let gate = new_spawn_operation_test_gate();
        crate::agent::install_startup_completion_test_gate(
            agent_name.to_owned(),
            Arc::clone(&gate.inner),
        );
        gate
    }

    pub fn spawn_operation_limits_for_test(&self) -> (usize, usize) {
        (
            MAX_CONCURRENT_SPAWN_OPERATIONS,
            SPAWN_OPERATION_QUEUE_CAPACITY,
        )
    }
}

#[cfg(feature = "test-support")]
fn new_spawn_operation_test_gate() -> InstalledSpawnOperationTestGate {
    let (entered_tx, entered_rx) = mpsc::unbounded_channel();
    InstalledSpawnOperationTestGate {
        inner: Arc::new(SpawnOperationTestGateInner {
            entered_tx,
            entered_rx: Mutex::new(entered_rx),
            release: Semaphore::new(0),
            panic_on_release: AtomicBool::new(false),
        }),
    }
}

#[cfg(feature = "test-support")]
pub(crate) async fn wait_for_spawn_operation_test_gate_inner(
    gate: &Arc<SpawnOperationTestGateInner>,
) {
    if gate.entered_tx.send(()).is_err() {
        return;
    }
    let Ok(permit) = gate.release.acquire().await else {
        return;
    };
    permit.forget();
    if gate.panic_on_release.swap(false, Ordering::SeqCst) {
        panic!("controlled spawn operation test panic");
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn notify_spawn_operation_test_gate_inner(gate: &Arc<SpawnOperationTestGateInner>) {
    let _ = gate.entered_tx.send(());
}

#[cfg(feature = "test-support")]
async fn wait_for_spawn_operation_test_gate(
    gate: &Arc<StdMutex<Option<Arc<SpawnOperationTestGateInner>>>>,
) {
    let gate = gate
        .lock()
        .expect("spawn operation test gate mutex poisoned")
        .clone();
    let Some(gate) = gate else {
        return;
    };
    wait_for_spawn_operation_test_gate_inner(&gate).await;
}

#[cfg(feature = "test-support")]
async fn wait_for_agent_name_test_gate(host: &HostHandle) -> Option<AgentNameTestCompletion> {
    let gate = host.state.lock().await.agent_name_test_gate.clone();
    let gate = gate?;
    let ordinal = gate.next_ordinal.fetch_add(1, Ordering::SeqCst);
    if gate.entered_tx.send(ordinal).is_err() {
        return None;
    }
    let Ok(permit) = gate.release.acquire().await else {
        return None;
    };
    permit.forget();
    if gate
        .panic_ordinals
        .lock()
        .expect("agent name gate panic set poisoned")
        .remove(&ordinal)
    {
        panic!("controlled agent name generation panic at ordinal {ordinal}");
    }
    Some(AgentNameTestCompletion { gate, ordinal })
}

#[cfg(not(feature = "test-support"))]
async fn wait_for_agent_name_test_gate(_host: &HostHandle) -> Option<AgentNameTestCompletion> {
    None
}

#[cfg(feature = "test-support")]
fn notify_agent_name_test_completion(completion: Option<AgentNameTestCompletion>) {
    if let Some(completion) = completion {
        let _ = completion.gate.completed_tx.send(completion.ordinal);
    }
}

#[cfg(not(feature = "test-support"))]
fn notify_agent_name_test_completion(_completion: Option<AgentNameTestCompletion>) {}

#[cfg(feature = "test-support")]
async fn wait_for_workbench_remove_test_hook(host: &HostHandle) {
    let hook = workbench_remove_hook_cell()
        .lock()
        .expect("workbench remove hook mutex poisoned")
        .clone();
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(feature = "test-support")]
fn notify_workbench_spawn_waiting_test_hook(host: &HostHandle) {
    let hook = workbench_remove_hook_cell()
        .lock()
        .expect("workbench remove hook mutex poisoned")
        .clone();
    if let Some(hook) = hook
        && hook.host_state_ptr == Arc::as_ptr(&host.state) as usize
    {
        hook.spawn_waiting.notify_one();
    }
}

#[cfg(not(feature = "test-support"))]
fn notify_workbench_spawn_waiting_test_hook(_host: &HostHandle) {}

#[cfg(not(feature = "test-support"))]
async fn wait_for_workbench_remove_test_hook(_host: &HostHandle) {}

#[cfg(feature = "test-support")]
fn workbench_remove_hook_cell() -> &'static std::sync::Mutex<Option<Arc<WorkbenchRemoveHook>>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<Option<Arc<WorkbenchRemoveHook>>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct InstalledTeamMutationAfterRefsHook {
    inner: Arc<TeamMutationAfterRefsHook>,
}

#[cfg(test)]
struct TeamMutationAfterRefsHook {
    host_state_ptr: usize,
    operation: &'static str,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
type TeamMutationAfterRefsHookCell = std::sync::Mutex<Option<Arc<TeamMutationAfterRefsHook>>>;

#[cfg(test)]
impl InstalledTeamMutationAfterRefsHook {
    async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }

    fn resume(&self) {
        self.inner.resume.notify_one();
    }
}

#[cfg(test)]
impl Drop for InstalledTeamMutationAfterRefsHook {
    fn drop(&mut self) {
        let mut hook = team_mutation_after_refs_hook_cell()
            .lock()
            .expect("team mutation hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(test)]
fn install_team_mutation_after_refs_test_hook(
    host: &HostHandle,
    operation: &'static str,
) -> InstalledTeamMutationAfterRefsHook {
    let inner = Arc::new(TeamMutationAfterRefsHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        operation,
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let mut hook = team_mutation_after_refs_hook_cell()
        .lock()
        .expect("team mutation hook mutex poisoned");
    assert!(hook.is_none(), "team mutation test hook already installed");
    *hook = Some(Arc::clone(&inner));
    InstalledTeamMutationAfterRefsHook { inner }
}

#[cfg(test)]
async fn wait_for_team_mutation_after_refs_test_hook(host: &HostHandle, operation: &'static str) {
    let hook = {
        team_mutation_after_refs_hook_cell()
            .lock()
            .expect("team mutation hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize || hook.operation != operation {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(test)]
fn team_mutation_after_refs_hook_cell() -> &'static TeamMutationAfterRefsHookCell {
    static HOOK: std::sync::OnceLock<TeamMutationAfterRefsHookCell> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct InstalledSpawnSessionRegistrationHook {
    inner: Arc<SpawnSessionRegistrationHook>,
}

#[cfg(test)]
struct SpawnSessionRegistrationHook {
    host_state_ptr: usize,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
type SpawnSessionRegistrationHookCell = std::sync::Mutex<Option<Arc<SpawnSessionRegistrationHook>>>;

#[cfg(test)]
impl InstalledSpawnSessionRegistrationHook {
    async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }

    fn resume(&self) {
        self.inner.resume.notify_one();
    }
}

#[cfg(test)]
impl Drop for InstalledSpawnSessionRegistrationHook {
    fn drop(&mut self) {
        let mut hook = spawn_session_registration_hook_cell()
            .lock()
            .expect("spawn session registration hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(test)]
fn install_spawn_session_registration_test_hook(
    host: &HostHandle,
) -> InstalledSpawnSessionRegistrationHook {
    let inner = Arc::new(SpawnSessionRegistrationHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let mut hook = spawn_session_registration_hook_cell()
        .lock()
        .expect("spawn session registration hook mutex poisoned");
    assert!(
        hook.is_none(),
        "spawn session registration hook already installed"
    );
    *hook = Some(Arc::clone(&inner));
    InstalledSpawnSessionRegistrationHook { inner }
}

#[cfg(test)]
async fn wait_for_spawn_session_registration_test_hook(host: &HostHandle) {
    let hook = {
        spawn_session_registration_hook_cell()
            .lock()
            .expect("spawn session registration hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(test)]
fn spawn_session_registration_hook_cell() -> &'static SpawnSessionRegistrationHookCell {
    static HOOK: std::sync::OnceLock<SpawnSessionRegistrationHookCell> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct InstalledStartupFailureFanoutRaceHook {
    inner: Arc<StartupFailureFanoutRaceHook>,
}

#[cfg(test)]
struct StartupFailureFanoutRaceHook {
    host_state_ptr: usize,
    winner: StartupFailureFanoutRaceWinner,
    ready: tokio::sync::Barrier,
    fanout_claimed: tokio::sync::Notify,
    failure_claimed: tokio::sync::Notify,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupFailureFanoutRaceWinner {
    Fanout,
    Failure,
}

#[cfg(test)]
type StartupFailureFanoutRaceHookCell = std::sync::Mutex<Option<Arc<StartupFailureFanoutRaceHook>>>;

#[cfg(test)]
impl InstalledStartupFailureFanoutRaceHook {
    async fn wait_until_ready(&self) {
        self.inner.ready.wait().await;
    }
}

#[cfg(test)]
impl Drop for InstalledStartupFailureFanoutRaceHook {
    fn drop(&mut self) {
        let mut hook = startup_failure_fanout_race_hook_cell()
            .lock()
            .expect("startup failure fanout race hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.fanout_claimed.notify_waiters();
        self.inner.failure_claimed.notify_waiters();
    }
}

#[cfg(test)]
fn install_startup_failure_fanout_race_test_hook(
    host: &HostHandle,
    winner: StartupFailureFanoutRaceWinner,
) -> InstalledStartupFailureFanoutRaceHook {
    let inner = Arc::new(StartupFailureFanoutRaceHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        winner,
        ready: tokio::sync::Barrier::new(3),
        fanout_claimed: tokio::sync::Notify::new(),
        failure_claimed: tokio::sync::Notify::new(),
    });
    let mut hook = startup_failure_fanout_race_hook_cell()
        .lock()
        .expect("startup failure fanout race hook mutex poisoned");
    assert!(
        hook.is_none(),
        "startup failure fanout race hook already installed"
    );
    *hook = Some(Arc::clone(&inner));
    InstalledStartupFailureFanoutRaceHook { inner }
}

#[cfg(test)]
async fn wait_before_startup_failure_fanout_test_hook(host: &HostHandle) {
    let hook = {
        startup_failure_fanout_race_hook_cell()
            .lock()
            .expect("startup failure fanout race hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize {
        return;
    }
    hook.ready.wait().await;
    if hook.winner == StartupFailureFanoutRaceWinner::Failure {
        hook.failure_claimed.notified().await;
    }
}

#[cfg(test)]
fn notify_startup_failure_fanout_claimed_test_hook(host: &HostHandle) {
    let hook = startup_failure_fanout_race_hook_cell()
        .lock()
        .expect("startup failure fanout race hook mutex poisoned")
        .clone();
    if let Some(hook) = hook
        && hook.host_state_ptr == Arc::as_ptr(&host.state) as usize
    {
        hook.fanout_claimed.notify_one();
    }
}

#[cfg(test)]
fn notify_startup_failure_claimed_test_hook(host: &HostHandle) {
    let hook = startup_failure_fanout_race_hook_cell()
        .lock()
        .expect("startup failure fanout race hook mutex poisoned")
        .clone();
    if let Some(hook) = hook
        && hook.host_state_ptr == Arc::as_ptr(&host.state) as usize
    {
        hook.failure_claimed.notify_one();
    }
}

#[cfg(test)]
async fn wait_for_startup_failure_fanout_race_test_hook(host: &HostHandle) {
    let hook = {
        startup_failure_fanout_race_hook_cell()
            .lock()
            .expect("startup failure fanout race hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize {
        return;
    }
    hook.ready.wait().await;
    if hook.winner == StartupFailureFanoutRaceWinner::Fanout {
        hook.fanout_claimed.notified().await;
    }
}

#[cfg(test)]
fn startup_failure_fanout_race_hook_cell() -> &'static StartupFailureFanoutRaceHookCell {
    static HOOK: std::sync::OnceLock<StartupFailureFanoutRaceHookCell> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct InstalledSpawnNewAgentFanoutHook {
    inner: Arc<SpawnNewAgentFanoutHook>,
}

#[cfg(test)]
struct SpawnNewAgentFanoutHook {
    host_state_ptr: usize,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    paused_once: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
type SpawnNewAgentFanoutHookCell = std::sync::Mutex<Option<Arc<SpawnNewAgentFanoutHook>>>;

#[cfg(test)]
impl InstalledSpawnNewAgentFanoutHook {
    async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }
}

#[cfg(test)]
impl Drop for InstalledSpawnNewAgentFanoutHook {
    fn drop(&mut self) {
        let mut hook = spawn_new_agent_fanout_hook_cell()
            .lock()
            .expect("spawn NewAgent fanout hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(test)]
fn install_spawn_new_agent_fanout_test_hook(host: &HostHandle) -> InstalledSpawnNewAgentFanoutHook {
    let inner = Arc::new(SpawnNewAgentFanoutHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
        paused_once: std::sync::atomic::AtomicBool::new(false),
    });
    let mut hook = spawn_new_agent_fanout_hook_cell()
        .lock()
        .expect("spawn NewAgent fanout hook mutex poisoned");
    assert!(
        hook.is_none(),
        "spawn NewAgent fanout hook already installed"
    );
    *hook = Some(Arc::clone(&inner));
    InstalledSpawnNewAgentFanoutHook { inner }
}

#[cfg(test)]
async fn wait_after_spawn_new_agent_fanout_test_hook(host: &HostHandle) {
    let hook = {
        spawn_new_agent_fanout_hook_cell()
            .lock()
            .expect("spawn NewAgent fanout hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize
        || hook.paused_once.swap(true, Ordering::SeqCst)
    {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(test)]
fn spawn_new_agent_fanout_hook_cell() -> &'static SpawnNewAgentFanoutHookCell {
    static HOOK: std::sync::OnceLock<SpawnNewAgentFanoutHookCell> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct InstalledSpawnVisibleBeforePublicationHook {
    inner: Arc<SpawnVisibleBeforePublicationHook>,
}

#[cfg(test)]
struct SpawnVisibleBeforePublicationHook {
    host_state_ptr: usize,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
type SpawnVisibleBeforePublicationHookCell =
    std::sync::Mutex<Option<Arc<SpawnVisibleBeforePublicationHook>>>;

#[cfg(test)]
impl InstalledSpawnVisibleBeforePublicationHook {
    async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }

    fn resume(&self) {
        self.inner.resume.notify_one();
    }
}

#[cfg(test)]
impl Drop for InstalledSpawnVisibleBeforePublicationHook {
    fn drop(&mut self) {
        let mut hook = spawn_visible_before_publication_hook_cell()
            .lock()
            .expect("spawn visible-before-publication hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(test)]
fn install_spawn_visible_before_publication_test_hook(
    host: &HostHandle,
) -> InstalledSpawnVisibleBeforePublicationHook {
    let inner = Arc::new(SpawnVisibleBeforePublicationHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let mut hook = spawn_visible_before_publication_hook_cell()
        .lock()
        .expect("spawn visible-before-publication hook mutex poisoned");
    assert!(
        hook.is_none(),
        "spawn visible-before-publication hook already installed"
    );
    *hook = Some(Arc::clone(&inner));
    InstalledSpawnVisibleBeforePublicationHook { inner }
}

#[cfg(test)]
async fn wait_for_spawn_visible_before_publication_test_hook(host: &HostHandle) {
    let hook = {
        spawn_visible_before_publication_hook_cell()
            .lock()
            .expect("spawn visible-before-publication hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(test)]
fn spawn_visible_before_publication_hook_cell() -> &'static SpawnVisibleBeforePublicationHookCell {
    static HOOK: std::sync::OnceLock<SpawnVisibleBeforePublicationHookCell> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct InstalledSpawnCancelledBeforeCleanupHook {
    inner: Arc<SpawnCancelledBeforeCleanupHook>,
}

#[cfg(test)]
struct SpawnCancelledBeforeCleanupHook {
    host_state_ptr: usize,
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
type SpawnCancelledBeforeCleanupHookCell =
    std::sync::Mutex<Option<Arc<SpawnCancelledBeforeCleanupHook>>>;

#[cfg(test)]
impl InstalledSpawnCancelledBeforeCleanupHook {
    async fn wait_until_reached(&self) {
        self.inner.reached.notified().await;
    }

    fn resume(&self) {
        self.inner.resume.notify_one();
    }
}

#[cfg(test)]
impl Drop for InstalledSpawnCancelledBeforeCleanupHook {
    fn drop(&mut self) {
        let mut hook = spawn_cancelled_before_cleanup_hook_cell()
            .lock()
            .expect("spawn cancelled-before-cleanup hook mutex poisoned");
        if hook
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
        {
            *hook = None;
        }
        self.inner.resume.notify_waiters();
    }
}

#[cfg(test)]
fn install_spawn_cancelled_before_cleanup_test_hook(
    host: &HostHandle,
) -> InstalledSpawnCancelledBeforeCleanupHook {
    let inner = Arc::new(SpawnCancelledBeforeCleanupHook {
        host_state_ptr: Arc::as_ptr(&host.state) as usize,
        reached: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let mut hook = spawn_cancelled_before_cleanup_hook_cell()
        .lock()
        .expect("spawn cancelled-before-cleanup hook mutex poisoned");
    assert!(
        hook.is_none(),
        "spawn cancelled-before-cleanup hook already installed"
    );
    *hook = Some(Arc::clone(&inner));
    InstalledSpawnCancelledBeforeCleanupHook { inner }
}

#[cfg(test)]
async fn wait_for_spawn_cancelled_before_cleanup_test_hook(host: &HostHandle) {
    let hook = {
        spawn_cancelled_before_cleanup_hook_cell()
            .lock()
            .expect("spawn cancelled-before-cleanup hook mutex poisoned")
            .clone()
    };
    let Some(hook) = hook else {
        return;
    };
    if hook.host_state_ptr != Arc::as_ptr(&host.state) as usize {
        return;
    }
    hook.reached.notify_one();
    hook.resume.notified().await;
}

#[cfg(test)]
fn spawn_cancelled_before_cleanup_hook_cell() -> &'static SpawnCancelledBeforeCleanupHookCell {
    static HOOK: std::sync::OnceLock<SpawnCancelledBeforeCleanupHookCell> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

pub(crate) struct HostSubAgentEmitter {
    spawn_tx: HostSubAgentSpawnTx,
    capacity_tx: HostCapacityTx,
    parent_agent_id: AgentId,
    workspace_roots: Vec<String>,
}

impl HostSubAgentEmitter {
    pub(crate) fn new(
        spawn_tx: HostSubAgentSpawnTx,
        capacity_tx: HostCapacityTx,
        parent_agent_id: AgentId,
        workspace_roots: Vec<String>,
    ) -> Self {
        Self {
            spawn_tx,
            capacity_tx,
            parent_agent_id,
            workspace_roots,
        }
    }
}

struct HostStorePaths {
    session: PathBuf,
    project: PathBuf,
    agent_team: PathBuf,
    review: PathBuf,
    settings: PathBuf,
    agents_view_preferences: PathBuf,
    custom_agent: PathBuf,
    mcp_server: PathBuf,
    steering: PathBuf,
    skills_index: PathBuf,
    skills_root_dir: PathBuf,
    mobile_pairings: PathBuf,
    workflow_runs: PathBuf,
}

impl SubAgentEmitter for HostSubAgentEmitter {
    fn on_backend_capacity(&self, backend_kind: BackendKind, state: BackendCapacityState) {
        let _ = self.capacity_tx.send(HostCapacityUpdate::Report {
            backend_kind,
            state,
        });
    }
    fn on_subagent_spawned(
        &self,
        tool_use_id: String,
        name: String,
        description: String,
        agent_type: String,
        session_id_hint: Option<SessionId>,
    ) -> Pin<Box<dyn Future<Output = Result<SubAgentHandle, String>> + Send + '_>> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.spawn_tx
                .send(HostSubAgentSpawnRequest {
                    parent_agent_id: self.parent_agent_id.clone(),
                    workspace_roots: self.workspace_roots.clone(),
                    tool_use_id,
                    name,
                    description,
                    agent_type,
                    session_id_hint,
                    reply: reply_tx,
                })
                .map_err(|_| {
                    format!(
                        "backend-native child spawn channel closed for parent {}",
                        self.parent_agent_id
                    )
                })?;
            reply_rx.await.map_err(|_| {
                format!(
                    "backend-native child spawn reply dropped for parent {}",
                    self.parent_agent_id
                )
            })?
        })
    }
}

impl HostHandle {
    pub(crate) async fn register_host_stream(
        &self,
        host_stream: Stream,
        agent_replay: AgentReplayMode,
    ) -> Vec<DeferredAgentAttachment> {
        let backend_setup = self.collect_backend_setup_respecting_probe().await;
        let mut state = self.state.lock().await;
        let host_path = host_stream.path().clone();

        let previous = state.host_streams.insert(
            host_path.clone(),
            HostSubscriber {
                stream: host_stream,
                bootstrapped: false,
                agent_replay,
                session_list_replay: SessionListReplayMode::for_agent_replay(agent_replay),
                session_list_snapshot: None,
                known_agent_streams: HashSet::new(),
                attached_agent_streams: HashSet::new(),
                bootstrapped_agent_streams: HashSet::new(),
                pending_bootstrap_new_agents: Vec::new(),
                pending_bootstrap_frames: Vec::new(),
                last_session_schemas: None,
                last_backend_config_schemas: None,
                last_backend_config_snapshots: None,
                last_backend_native_settings_snapshots: None,
                last_backend_capacity: None,
                capacity_replay_ready: false,
                last_launch_profile_catalog: None,
            },
        );
        assert!(
            previous.is_none(),
            "duplicate host stream registration for {}",
            host_path
        );

        // First-run: enable whichever backends are already installed so a fresh
        // install lands on a usable picker instead of an empty one. No-op once a
        // settings file exists.
        {
            let installed: Vec<protocol::BackendKind> = backend_setup
                .backends
                .iter()
                .filter(|info| info.status == protocol::BackendSetupStatus::Installed)
                .map(|info| info.backend_kind)
                .collect();
            match state
                .settings_store
                .lock()
                .await
                .seed_installed_backends_if_fresh(&installed)
            {
                Ok(true) => tracing::info!(
                    ?installed,
                    "seeded enabled backends from installed CLIs on first run"
                ),
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(%err, "failed to seed installed backends on first run")
                }
            }
        }

        // Pare-back: drop deprecated builtin custom agents (the old specialist
        // set) when the stored copy is an unedited published version and no
        // team member references it. User-edited copies and agents in use by
        // teams are preserved. Idempotent, so re-running per registration is
        // harmless.
        {
            let referenced: std::collections::HashSet<String> =
                match state.team_registry.snapshot().await {
                    Ok(snapshot) => snapshot
                        .members
                        .iter()
                        .filter_map(|member| member.custom_agent_id.as_ref())
                        .map(|id| id.0.clone())
                        .collect(),
                    Err(err) => {
                        tracing::warn!(%err, "skipping builtin agent pare-back: no team snapshot");
                        std::collections::HashSet::new()
                    }
                };
            let custom_agents = state.custom_agent_store.lock().await;
            for id in crate::store::custom_agents::deprecated_builtin_custom_agent_ids() {
                if referenced.contains(id) {
                    continue;
                }
                let agent_id = protocol::CustomAgentId(id.to_owned());
                let Some(record) = custom_agents.get(&agent_id) else {
                    continue;
                };
                if !crate::store::custom_agents::is_superseded_builtin(&record) {
                    continue;
                }
                match custom_agents.delete(&agent_id) {
                    Ok(_) => tracing::info!(%agent_id, "removed deprecated builtin custom agent"),
                    Err(err) => {
                        tracing::warn!(%agent_id, %err, "failed to remove deprecated builtin")
                    }
                }
            }
        }

        let settings = state
            .settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for registration: {err}"));
        let refresh_dynamic_session_schemas = settings
            .enabled_backends
            .iter()
            .copied()
            .any(backend_has_dynamic_session_schema);
        let schemas = session_schemas_for_enabled_backends(&state, &settings.enabled_backends);
        let backend_config_schemas = crate::backend::backend_config_schema_catalog();
        let backend_config_snapshots = state.backend_config_snapshots.clone();
        let launch_profile_catalog = launch_profile_catalog_for_settings(&state, &settings);

        let mobile_access = {
            let Some(subscriber) = state.host_streams.get(&host_path) else {
                panic!(
                    "host stream {} disappeared during mobile access bootstrap registration",
                    host_path
                );
            };
            match state
                .mobile_access
                .register_bootstrap_subscriber(subscriber.stream.clone())
                .await
            {
                Ok(payload) => payload,
                Err(_) => {
                    state.host_streams.remove(&host_path);
                    return Vec::new();
                }
            }
        };

        let projects = state
            .project_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list projects for host registration: {err}"));
        let project_ids = projects
            .iter()
            .map(|project| project.id.clone())
            .collect::<Vec<_>>();

        let session_list_scope = {
            let subscriber = state.host_streams.get(&host_path).unwrap_or_else(|| {
                panic!("host stream {host_path} disappeared before session bootstrap scope")
            });
            subscriber.session_list_replay.default_scope()
        };
        let session_summaries = state
            .session_store
            .lock()
            .await
            .summaries_for_scope_with_antigravity_conversations_dir(
                session_list_scope,
                &state.antigravity_conversations_dir,
            )
            .unwrap_or_else(|err| panic!("failed to list sessions for host registration: {err}"));
        let (sessions, session_list) = {
            let subscriber = state.host_streams.get_mut(&host_path).unwrap_or_else(|| {
                panic!("host stream {host_path} disappeared before session bootstrap paging")
            });
            replace_session_list_snapshot(
                subscriber,
                session_list_scope,
                session_summaries,
                None,
                "host_bootstrap",
            )
            .unwrap_or_else(|err| panic!("failed to page sessions for host registration: {err}"))
        };

        let mcp_servers = state
            .mcp_server_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| {
                panic!("failed to list MCP servers for host registration: {err}")
            });

        let skills = {
            let store = state.skill_store.lock().await;
            if let Err(err) = store.sync_from_disk() {
                tracing::warn!(
                    host_stream = %host_path,
                    error = %err,
                    "failed to sync skills for host registration; continuing with last known state"
                );
            }
            match store.list() {
                Ok(skills) => skills,
                Err(err) => {
                    tracing::warn!(
                        host_stream = %host_path,
                        error = %err,
                        "failed to list skills for host registration; continuing without skills"
                    );
                    Vec::new()
                }
            }
        };

        let steering = state
            .steering_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| panic!("failed to list steering for host registration: {err}"));

        let custom_agents = state
            .custom_agent_store
            .lock()
            .await
            .list()
            .unwrap_or_else(|err| {
                panic!("failed to list custom agents for host registration: {err}")
            });

        let team_snapshot = match state.team_registry.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::error!(
                    host_stream = %host_path,
                    error = %err,
                    "failed to snapshot teams for host registration"
                );
                state.mobile_access.unregister_subscriber(host_path.clone());
                state.host_streams.remove(&host_path);
                return Vec::new();
            }
        };
        let workflow_summaries = state.workflow_catalog.summaries();
        let workflow_diagnostics = state.workflow_catalog.diagnostics();
        let workflow_runs = state.workflow_run_store.list();
        let workflow_locations = state.workflow_locations.clone();
        let agents_view_preferences = match state.agents_view_preferences_store.as_ref() {
            Some(store) => {
                let mut snapshot = store.lock().await.snapshot();
                complete_agents_view_preferences_snapshot(&state, &mut snapshot).await;
                Some(snapshot)
            }
            None => None,
        };

        let agent_ids = state.registry.agent_ids();
        let agent_visibility = state.agent_visibility.clone();
        let mut agents = Vec::new();
        let mut deferred_attachments = Vec::new();
        for agent_id in agent_ids {
            if !agent_visibility
                .bootstrap_eligible(&agent_id, state.agent_sessions.contains_key(&agent_id))
            {
                continue;
            }
            let agent_handle = state.registry.agent_handle(&agent_id).unwrap_or_else(|| {
                panic!(
                    "registry missing handle for listed agent {} during host stream registration",
                    agent_id
                )
            });
            let start = agent_handle.snapshot();
            let activity_summary = current_agent_activity_summary_state(&state, &start.agent_id);
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared during registration bootstrap build",
                    host_path
                );
            };
            let instance_stream = new_instance_stream(&start.agent_id);
            let new_agent = NewAgentPayload {
                agent_id: start.agent_id.clone(),
                name: start.name.clone(),
                origin: start.origin,
                backend_kind: start.backend_kind,
                launch_profile_id: start.launch_profile_id.clone(),
                workspace_roots: start.workspace_roots.clone(),
                custom_agent_id: start.custom_agent_id.clone(),
                team_id: start.team_id.clone(),
                team_member_id: start.team_member_id.clone(),
                project_id: start.project_id.clone(),
                parent_agent_id: start.parent_agent_id.clone(),
                session_id: start.session_id.clone(),
                workflow: start.workflow.clone(),
                created_at_ms: start.created_at_ms,
                instance_stream: instance_stream.clone(),
                activity_summary,
            };
            subscriber
                .known_agent_streams
                .insert(instance_stream.clone());
            let attach_eagerly = matches!(subscriber.agent_replay, AgentReplayMode::Eager);
            let agent_stream = subscriber.stream.with_path(instance_stream.clone());
            if attach_eagerly {
                subscriber
                    .attached_agent_streams
                    .insert(instance_stream.clone());
            }
            agents.push(new_agent);
            if attach_eagerly {
                deferred_attachments.push(DeferredAgentAttachment {
                    host_stream: host_path.clone(),
                    agent_stream: instance_stream.clone(),
                    reply: None,
                    agent_handle: Some(agent_handle),
                    stream: Some(agent_stream),
                });
            }
        }
        let (usage_handles, closed_usage_snapshots, live_usage_agent_ids, usage_agent_sessions) =
            task_token_usage_sources_for_state(&state);
        drop(state);
        let task_token_usages = task_token_usage_rollups_from_handles(
            usage_handles,
            closed_usage_snapshots,
            &live_usage_agent_ids,
            &usage_agent_sessions,
        )
        .await;

        let bootstrap = HostBootstrapPayload {
            settings,
            mobile_access,
            backend_setup,
            session_schemas: schemas,
            backend_config_schemas,
            backend_config_snapshots,
            launch_profile_catalog,
            sessions,
            session_list,
            projects,
            mcp_servers,
            skills,
            steering,
            custom_agents,
            team_preset_catalog: team_snapshot.catalog,
            team_drafts: team_snapshot.drafts,
            teams: team_snapshot.teams,
            team_members: team_snapshot.members,
            team_member_bindings: team_snapshot.bindings,
            agents,
            task_token_usages,
            workflow_summaries,
            workflow_diagnostics,
            workflow_runs,
            workflow_locations,
            agents_view_preferences,
        };

        let payload = serde_json::to_value(&bootstrap)
            .expect("failed to serialize HostBootstrap payload for host stream registration");
        let bootstrap_agent_ids = bootstrap
            .agents
            .iter()
            .map(|agent| agent.agent_id.clone())
            .collect::<HashSet<_>>();
        let mut state = self.state.lock().await;
        let agent_visibility = state.agent_visibility.clone();
        let (pending_new_agents, pending_frames, bootstrap_stream) = {
            let Some(subscriber) = state.host_streams.get_mut(&host_path) else {
                panic!(
                    "host stream {} disappeared before HostBootstrap emission",
                    host_path
                );
            };
            if subscriber
                .stream
                .send_value(FrameKind::HostBootstrap, payload)
                .is_err()
            {
                state.mobile_access.unregister_subscriber(host_path.clone());
                state.host_streams.remove(&host_path);
                state.agent_visibility.remove_host_stream(&host_path);
                return Vec::new();
            }
            for agent in &bootstrap.agents {
                agent_visibility.record_new_agent(agent.agent_id.clone(), host_path.clone());
            }
            subscriber.bootstrapped = true;
            subscriber.last_session_schemas = Some(bootstrap.session_schemas.clone());
            subscriber.last_backend_config_schemas = Some(bootstrap.backend_config_schemas.clone());
            subscriber.last_backend_config_snapshots =
                Some(bootstrap.backend_config_snapshots.clone());
            subscriber.last_backend_native_settings_snapshots = Some(Vec::new());
            subscriber.last_launch_profile_catalog = Some(bootstrap.launch_profile_catalog.clone());
            (
                std::mem::take(&mut subscriber.pending_bootstrap_new_agents),
                std::mem::take(&mut subscriber.pending_bootstrap_frames),
                subscriber.stream.clone(),
            )
        };
        for pending in pending_new_agents {
            if bootstrap_agent_ids.contains(&pending.start.agent_id) {
                continue;
            }
            match emit_new_agent_for_stream(
                &pending.start,
                &pending.agent_handle,
                &bootstrap_stream,
                pending.instance_stream,
                pending.attach_eagerly,
                pending.activity_summary,
            ) {
                Ok(Some(attachment)) => {
                    agent_visibility
                        .record_new_agent(pending.start.agent_id.clone(), host_path.clone());
                    deferred_attachments.push(attachment);
                }
                Ok(None) => {
                    agent_visibility
                        .record_new_agent(pending.start.agent_id.clone(), host_path.clone());
                }
                Err(_) => {
                    state.mobile_access.unregister_subscriber(host_path.clone());
                    state.host_streams.remove(&host_path);
                    state.agent_visibility.remove_host_stream(&host_path);
                    return Vec::new();
                }
            }
        }
        for (kind, payload) in pending_frames {
            if bootstrap_stream.send_value(kind, payload).is_err() {
                state.mobile_access.unregister_subscriber(host_path.clone());
                state.host_streams.remove(&host_path);
                state.agent_visibility.remove_host_stream(&host_path);
                return Vec::new();
            }
        }
        state
            .mobile_access
            .activate_bootstrap_subscriber(host_path.clone());

        for project_id in project_ids {
            if let Err(error) =
                subscribe_host_to_project(&mut state, &host_path, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to subscribe host to project stream during registration"
                );
            }
        }

        drop(state);
        if refresh_dynamic_session_schemas {
            self.schedule_session_schema_refresh();
        }
        self.schedule_backend_config_snapshot_refresh();
        deferred_attachments
    }

    pub(crate) async fn attach_deferred_agent_stream(&self, attachment: DeferredAgentAttachment) {
        let DeferredAgentAttachment {
            host_stream,
            agent_stream,
            reply,
            agent_handle,
            stream,
        } = attachment;
        let reply = match reply {
            Some(reply) => Some(reply),
            None => match (agent_handle, stream) {
                (Some(agent_handle), Some(stream)) => agent_handle.begin_attach(stream),
                (None, None) => None,
                _ => unreachable!("deferred agent attachment source must be complete"),
            },
        };
        let attached = match reply {
            Some(reply) => reply.await.unwrap_or(false),
            None => false,
        };
        let mut state = self.state.lock().await;
        let Some(subscriber) = state.host_streams.get_mut(&host_stream) else {
            return;
        };
        if attached {
            subscriber.bootstrapped_agent_streams.insert(agent_stream);
        } else {
            subscriber.attached_agent_streams.remove(&agent_stream);
            subscriber.bootstrapped_agent_streams.remove(&agent_stream);
        }
    }

    /// Replays the typed capacity snapshot after the canonical host bootstrap
    /// sequence has yielded to the connection's first command or idle replay.
    pub(crate) async fn replay_backend_capacity_for_host_stream(&self, path: &StreamPath) {
        let mut state = self.state.lock().await;
        let snapshots = backend_capacity_snapshots(&state);
        let Some(subscriber) = state.host_streams.get_mut(path) else {
            return;
        };
        subscriber.capacity_replay_ready = true;
        if emit_backend_capacity_for_subscriber(&snapshots, subscriber).is_err() {
            state.host_streams.remove(path);
            state.agent_visibility.remove_host_stream(path);
        }
    }

    pub(crate) async fn unregister_host_stream(&self, path: &StreamPath) {
        let (project_handles, terminals, review_registry) = {
            let mut state = self.state.lock().await;
            state.host_streams.remove(path);
            state.agent_visibility.remove_host_stream(path);
            state.mobile_access.unregister_subscriber(path.clone());
            let review_registry = state.review_registry.clone();
            let project_handles = state
                .project_streams
                .values()
                .map(|subscription| subscription.handle.clone())
                .collect::<Vec<_>>();

            let browse_keys = state
                .browse_streams
                .keys()
                .filter(|(host_stream, _)| host_stream == path)
                .cloned()
                .collect::<Vec<_>>();
            for key in browse_keys {
                state.browse_streams.remove(&key);
            }

            let terminal_keys = state
                .terminal_streams
                .keys()
                .filter(|(host_stream, _)| host_stream == path)
                .cloned()
                .collect::<Vec<_>>();
            let mut terminals = Vec::with_capacity(terminal_keys.len());
            for key in terminal_keys {
                let Some(terminal) = state.terminal_streams.remove(&key) else {
                    continue;
                };
                terminals.push(terminal);
            }
            (project_handles, terminals, review_registry)
        };

        for handle in project_handles {
            handle.remove_subscriber(path.clone()).await;
        }

        review_registry.unsubscribe_all(path.clone()).await;

        for terminal in terminals {
            terminal.close().await;
        }
    }

    async fn workbench_parent_lock(&self, parent_project_id: &ProjectId) -> Arc<Mutex<()>> {
        let mut state = self.state.lock().await;
        state
            .workbench_parent_locks
            .retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = state
            .workbench_parent_locks
            .get(parent_project_id)
            .and_then(Weak::upgrade)
        {
            return lock;
        }

        let lock = Arc::new(Mutex::new(()));
        state
            .workbench_parent_locks
            .insert(parent_project_id.clone(), Arc::downgrade(&lock));
        lock
    }

    #[cfg(test)]
    pub(crate) async fn spawn_agent(&self, payload: SpawnAgentPayload) -> AppResult<AgentId> {
        self.spawn_agent_with_origin(payload, AgentOrigin::User)
            .await
    }

    fn schedule_generated_agent_name(
        &self,
        agent_handle: AgentHandle,
        request: GenerateAgentNameRequest,
    ) {
        let host = self.clone();
        tokio::spawn(async move {
            let test_completion = wait_for_agent_name_test_gate(&host).await;
            let result = await_agent_name_generation(
                generate_agent_name(request),
                AGENT_NAME_GENERATION_TIMEOUT,
            )
            .await;
            match result {
                Ok(name) if !name.trim().is_empty() => {
                    if agent_handle.apply_generated_name(Ok(name)).await == Some(true) {
                        host.fan_out_session_lists().await;
                    }
                }
                Ok(_) => {
                    tracing::warn!(
                        agent_id = %agent_handle.snapshot().agent_id,
                        "automatic agent name generation returned an empty name"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        agent_id = %agent_handle.snapshot().agent_id,
                        %error,
                        "automatic agent name generation failed"
                    );
                }
            }
            notify_agent_name_test_completion(test_completion);
        });
    }

    async fn spawn_agent_for_operation(
        &self,
        payload: SpawnAgentPayload,
        terminal_claim: SpawnOperationTerminalClaim,
    ) -> AppResult<AgentId> {
        self.spawn_agent_with_origin_config_and_team(
            payload,
            AgentOrigin::User,
            None,
            None,
            None,
            Some(terminal_claim),
        )
        .await
    }

    pub(crate) fn start_spawn_agent_operation(
        &self,
        payload: SpawnAgentPayload,
        request_stream: StreamPath,
        output_stream: Stream,
    ) -> AppResult<()> {
        let Some(owner) = self.spawn_operations.owner.upgrade() else {
            return Err(AppError::internal(
                "spawn_agent",
                anyhow!("host spawn operation owner is unavailable"),
            ));
        };
        if owner.cancel.is_cancelled() {
            return Err(AppError::internal(
                "spawn_agent",
                anyhow!("host spawn operation owner is shut down"),
            ));
        }
        self.spawn_operations
            .tx
            .try_send(SpawnOperation {
                payload,
                request_stream,
                output_stream,
            })
            .map_err(|error| {
                AppError::internal(
                    "spawn_agent",
                    anyhow!("host spawn operation capacity unavailable: {error}"),
                )
            })
    }

    pub async fn shutdown_spawn_operations(&self) {
        if let Some(owner) = self.spawn_operations.owner.upgrade() {
            owner.shutdown().await;
        }
    }

    pub(crate) async fn compact_agent_in_background(
        &self,
        agent_id: AgentId,
        payload: AgentCompactPayload,
        stream: Stream,
    ) -> AppResult<()> {
        let Some(compaction) = self
            .begin_agent_compaction(agent_id, payload, stream, None)
            .await?
        else {
            return Ok(());
        };
        let host = self.clone();
        tokio::spawn(async move {
            host.finish_agent_compaction(compaction).await;
        });
        Ok(())
    }

    async fn compact_agent_if_inactive_in_background(
        &self,
        agent_id: AgentId,
        expected_activity_counter: u64,
        expected_supervisor_settings_epoch: u64,
        payload: AgentCompactPayload,
        stream: Stream,
    ) -> AppResult<bool> {
        let supervisor_settings_rx = self.supervisor_settings_receiver().await;
        let Some(compaction) = self
            .begin_agent_compaction(
                agent_id,
                payload,
                stream,
                Some((
                    expected_activity_counter,
                    expected_supervisor_settings_epoch,
                    supervisor_settings_rx,
                )),
            )
            .await?
        else {
            return Ok(false);
        };
        let host = self.clone();
        tokio::spawn(async move {
            host.finish_agent_compaction(compaction).await;
        });
        Ok(true)
    }

    pub(crate) async fn compact_team(
        &self,
        payload: TeamCompactPayload,
        stream: Stream,
    ) -> AppResult<()> {
        let targets = self.team_compact_targets(&payload.team_id).await?;
        let member_ids = targets
            .iter()
            .map(|target| target.member_id.clone())
            .collect::<Vec<_>>();
        let agent_ids = targets
            .iter()
            .map(|target| target.agent_id.clone())
            .collect::<Vec<_>>();
        let mut compactions = Vec::with_capacity(targets.len());
        let mut results = Vec::new();
        for target in targets {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let target_agent_id = target.agent_id.clone();
            let agent_stream = Stream::new(
                StreamPath(format!("/agent/{}/team-compact", target_agent_id)),
                tx,
            );
            let agent_payload = AgentCompactPayload {
                summary_prompt: payload.summary_prompt.clone(),
                max_summary_bytes: payload.max_summary_bytes,
            };
            match self
                .begin_agent_compaction(target_agent_id.clone(), agent_payload, agent_stream, None)
                .await?
            {
                Some(compaction) => {
                    compactions.push(TeamAgentCompaction {
                        target,
                        compaction,
                        rx,
                    });
                }
                None => {
                    results.push(drain_final_agent_compact_notify(&target_agent_id, &mut rx));
                }
            }
        }

        send_team_compact_notify(
            &stream,
            TeamCompactNotifyPayload {
                status: TeamCompactStatus::Started,
                team_id: payload.team_id.clone(),
                member_ids: member_ids.clone(),
                agent_ids: agent_ids.clone(),
                results: Vec::new(),
                message: None,
            },
        );

        let host = self.clone();
        tokio::spawn(async move {
            host.finish_team_compactions(
                payload,
                member_ids,
                agent_ids,
                compactions,
                results,
                stream,
            )
            .await;
        });
        Ok(())
    }

    async fn team_compact_targets(&self, team_id: &TeamId) -> AppResult<Vec<TeamCompactTarget>> {
        const OPERATION: &str = "team_compact";
        let registry = { self.state.lock().await.team_registry.clone() };
        let snapshot = registry
            .snapshot()
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        if !snapshot.teams.iter().any(|team| team.id == *team_id) {
            return Err(AppError::not_found(
                OPERATION,
                format!("team {team_id} does not exist"),
            ));
        }

        let members = snapshot
            .members
            .into_iter()
            .filter(|member| member.team_id == *team_id && member.state == TeamMemberState::Active)
            .collect::<Vec<_>>();
        let bindings = snapshot
            .bindings
            .into_iter()
            .map(|binding| (binding.member_id.clone(), binding))
            .collect::<HashMap<_, _>>();
        let mut targets = Vec::new();
        for member in members {
            let Some(binding) = bindings.get(&member.id) else {
                return Err(AppError::internal_message(
                    OPERATION,
                    format!("team member {} has no binding", member.id),
                    anyhow!("team member {} has no binding", member.id),
                ));
            };
            if binding.status != AgentControlStatus::Idle {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "team member {} is not idle ({:?})",
                        member.id, binding.status
                    ),
                ));
            }
            let Some(agent_id) = binding.current_agent_id.clone() else {
                continue;
            };
            let status = self.agent_status_snapshot(&agent_id).await.ok_or_else(|| {
                AppError::conflict(
                    OPERATION,
                    format!(
                        "team member {} is bound to missing agent {agent_id}",
                        member.id
                    ),
                )
            })?;
            if status.terminated {
                return Err(AppError::conflict(
                    OPERATION,
                    format!("team member {} agent {agent_id} is terminated", member.id),
                ));
            }
            if status.is_active() || status.is_plan_approval_pending() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!("team member {} agent {agent_id} is not idle", member.id),
                ));
            }
            targets.push(TeamCompactTarget {
                member_id: member.id,
                agent_id,
            });
        }

        if targets.is_empty() {
            return Err(AppError::conflict(
                OPERATION,
                format!("team {team_id} has no live idle agents to compact"),
            ));
        }
        Ok(targets)
    }

    async fn finish_team_compactions(
        &self,
        payload: TeamCompactPayload,
        member_ids: Vec<TeamMemberId>,
        agent_ids: Vec<AgentId>,
        compactions: Vec<TeamAgentCompaction>,
        mut results: Vec<AgentCompactNotifyPayload>,
        stream: Stream,
    ) {
        let mut handles = Vec::with_capacity(compactions.len());
        for compaction in compactions {
            let host = self.clone();
            let target = compaction.target.clone();
            handles.push((
                target,
                tokio::spawn(async move { host.finish_team_compaction_target(compaction).await }),
            ));
        }

        for (target, handle) in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(error) => results.push(AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: target.agent_id,
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some(format!("team compaction task failed: {error}")),
                }),
            }
        }

        let failures = results
            .iter()
            .filter(|result| result.status != AgentCompactStatus::Completed)
            .count();
        let (status, message) = if failures == 0 {
            (TeamCompactStatus::Completed, None)
        } else {
            (
                TeamCompactStatus::Failed,
                Some(format!(
                    "{failures} of {} team agents failed to compact",
                    results.len()
                )),
            )
        };
        send_team_compact_notify(
            &stream,
            TeamCompactNotifyPayload {
                status,
                team_id: payload.team_id,
                member_ids,
                agent_ids,
                results,
                message,
            },
        );
    }

    async fn finish_team_compaction_target(
        &self,
        team_compaction: TeamAgentCompaction,
    ) -> AgentCompactNotifyPayload {
        let old_agent_id = team_compaction.target.agent_id.clone();
        self.finish_agent_compaction(team_compaction.compaction)
            .await;
        let mut rx = team_compaction.rx;
        drain_final_agent_compact_notify(&old_agent_id, &mut rx)
    }

    #[cfg(test)]
    pub(crate) async fn compact_agent(
        &self,
        agent_id: AgentId,
        payload: AgentCompactPayload,
        stream: Stream,
    ) -> AppResult<()> {
        let Some(compaction) = self
            .begin_agent_compaction(agent_id, payload, stream, None)
            .await?
        else {
            return Ok(());
        };
        self.finish_agent_compaction(compaction).await;
        Ok(())
    }

    async fn begin_agent_compaction(
        &self,
        agent_id: AgentId,
        payload: AgentCompactPayload,
        stream: Stream,
        inactivity_gate: Option<(u64, u64, watch::Receiver<SupervisorSettingsSignal>)>,
    ) -> AppResult<Option<AgentCompaction>> {
        let summary_prompt = payload
            .summary_prompt
            .unwrap_or_else(default_compaction_summary_prompt);
        let max_summary_bytes = payload
            .max_summary_bytes
            .map(|bytes| bytes as usize)
            .unwrap_or(DEFAULT_COMPACTION_SUMMARY_MAX_BYTES)
            .clamp(1, MAX_COMPACTION_SUMMARY_BYTES);

        let Some((agent_handle, start, old_session_id, access_mode, session_store, team_registry)) =
            ({
                let state = self.state.lock().await;
                state.registry.agent_handle(&agent_id).map(|agent_handle| {
                    let start = agent_handle.snapshot();
                    (
                        agent_handle,
                        start,
                        state.agent_sessions.get(&agent_id).cloned(),
                        state.registry.agent_access_mode(&agent_id),
                        Arc::clone(&state.session_store),
                        state.team_registry.clone(),
                    )
                })
            })
        else {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent is not running".to_owned()),
                },
            );
            return Ok(None);
        };

        let Some(old_session_id) = old_session_id else {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent has no session to compact".to_owned()),
                },
            );
            return Ok(None);
        };
        let Some(access_mode) = access_mode else {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent access mode is unavailable".to_owned()),
                },
            );
            return Ok(None);
        };
        if start.origin == AgentOrigin::BackendNative {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("backend-native agents cannot be compacted".to_owned()),
                },
            );
            return Ok(None);
        }
        let old_record = match session_store.lock().await.get(&old_session_id) {
            Some(record) => record,
            None => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("agent session metadata is missing".to_owned()),
                    },
                );
                return Ok(None);
            }
        };
        if old_record.compacted_to_session_id.is_some() {
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some("agent session is already compacted".to_owned()),
                },
            );
            return Ok(None);
        }

        let compaction_start = match inactivity_gate {
            Some((
                expected_activity_counter,
                expected_supervisor_settings_epoch,
                supervisor_settings_rx,
            )) => {
                agent_handle
                    .begin_compact_if_inactive(
                        expected_activity_counter,
                        expected_supervisor_settings_epoch,
                        supervisor_settings_rx,
                        summary_prompt,
                        max_summary_bytes,
                    )
                    .await
            }
            None => agent_handle.begin_compact(summary_prompt, max_summary_bytes),
        };
        let summary_rx = match compaction_start {
            CompactionStart::Started(summary_rx) => summary_rx,
            CompactionStart::Rejected(error) => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some(error),
                    },
                );
                return Ok(None);
            }
            CompactionStart::Closed => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("agent stopped before compaction completed".to_owned()),
                    },
                );
                return Ok(None);
            }
        };
        send_agent_compact_notify(
            &stream,
            AgentCompactNotifyPayload {
                status: AgentCompactStatus::Started,
                old_agent_id: agent_id.clone(),
                old_session_id: Some(old_session_id.clone()),
                new_agent_id: None,
                new_session_id: None,
                summary_preview: None,
                message: None,
            },
        );
        Ok(Some(AgentCompaction {
            agent_id,
            agent_handle,
            start,
            old_session_id,
            access_mode,
            old_record,
            session_store,
            team_registry,
            stream,
            summary_rx,
        }))
    }

    async fn finish_agent_compaction(&self, compaction: AgentCompaction) {
        let AgentCompaction {
            agent_id,
            agent_handle,
            start,
            old_session_id,
            access_mode,
            old_record,
            session_store,
            team_registry,
            stream,
            summary_rx,
        } = compaction;
        let summary = match summary_rx.await {
            Ok(Ok(summary)) => summary,
            Ok(Err(error)) => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some(error),
                    },
                );
                return;
            }
            Err(_) => {
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: None,
                        message: Some("agent stopped before compaction completed".to_owned()),
                    },
                );
                return;
            }
        };
        if summary.session_id != old_session_id {
            let _ = agent_handle.release_compaction().await;
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some(format!(
                        "compaction summary came from unexpected session {}",
                        summary.session_id
                    )),
                },
            );
            return;
        }

        let summary_preview = compaction_summary_preview(&summary.summary);
        let replacement_prompt = build_compaction_replacement_prompt(&summary.summary);
        let team_context = match (
            start.origin,
            start.team_id.clone(),
            start.team_member_id.clone(),
        ) {
            (AgentOrigin::TeamMember, Some(team_id), Some(team_member_id)) => {
                Some(TeamSpawnContext {
                    team_id,
                    team_member_id,
                })
            }
            (AgentOrigin::TeamMember, _, _) => {
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some("team agent metadata is incomplete".to_owned()),
                    },
                );
                return;
            }
            (_, None, None) => None,
            _ => {
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some("team agent metadata is incomplete".to_owned()),
                    },
                );
                return;
            }
        };
        let replacement_payload = SpawnAgentPayload {
            name: Some(start.name.clone()),
            custom_agent_id: start.custom_agent_id.clone(),
            parent_agent_id: start.parent_agent_id.clone(),
            project_id: start.project_id.clone(),
            params: SpawnAgentParams::New {
                workspace_roots: start.workspace_roots.clone(),
                prompt: replacement_prompt,
                images: None,
                backend_kind: start.backend_kind,
                launch_profile_id: old_record.launch_profile_id.clone(),
                cost_hint: None,
                access_mode,
                session_settings: old_record.session_settings.clone(),
            },
        };
        let new_agent_id = self
            .spawn_agent_with_origin_config_and_team(
                replacement_payload,
                start.origin,
                None,
                team_context.clone(),
                None,
                None,
            )
            .await;
        let new_agent_id = match new_agent_id {
            Ok(agent_id) => agent_id,
            Err(error) => {
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: None,
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some(format!("replacement spawn failed: {error}")),
                    },
                );
                return;
            }
        };
        let new_session_id = match self.wait_for_agent_session_id_result(&new_agent_id).await {
            Ok(session_id) => session_id,
            Err(error) => {
                self.close_agent(&new_agent_id).await;
                let _ = agent_handle.release_compaction().await;
                send_agent_compact_notify(
                    &stream,
                    AgentCompactNotifyPayload {
                        status: AgentCompactStatus::Failed,
                        old_agent_id: agent_id,
                        old_session_id: Some(old_session_id),
                        new_agent_id: Some(new_agent_id),
                        new_session_id: None,
                        summary_preview: Some(summary_preview),
                        message: Some(format!("replacement agent failed to start: {error}")),
                    },
                );
                return;
            }
        };

        if let Some(context) = team_context.as_ref() {
            let refs_result = {
                let state = self.state.lock().await;
                agent_team_validation_refs(&state, "agent_compact").await
            };
            let refs = match refs_result {
                Ok(refs) => refs,
                Err(error) => {
                    self.close_agent(&new_agent_id).await;
                    let _ = agent_handle.release_compaction().await;
                    send_agent_compact_notify(
                        &stream,
                        AgentCompactNotifyPayload {
                            status: AgentCompactStatus::Failed,
                            old_agent_id: agent_id,
                            old_session_id: Some(old_session_id),
                            new_agent_id: Some(new_agent_id),
                            new_session_id: Some(new_session_id),
                            summary_preview: Some(summary_preview),
                            message: Some(format!("team validation failed: {error}")),
                        },
                    );
                    return;
                }
            };
            match team_registry
                .rotate_member_agent(
                    context.team_member_id.clone(),
                    agent_id.clone(),
                    new_agent_id.clone(),
                    old_session_id.clone(),
                    new_session_id.clone(),
                    refs,
                )
                .await
            {
                Ok(events) => {
                    let mut state = self.state.lock().await;
                    fan_out_team_registry_events(&mut state, events).await;
                }
                Err(error) => {
                    self.close_agent(&new_agent_id).await;
                    let _ = agent_handle.release_compaction().await;
                    send_agent_compact_notify(
                        &stream,
                        AgentCompactNotifyPayload {
                            status: AgentCompactStatus::Failed,
                            old_agent_id: agent_id,
                            old_session_id: Some(old_session_id),
                            new_agent_id: Some(new_agent_id),
                            new_session_id: Some(new_session_id),
                            summary_preview: Some(summary_preview),
                            message: Some(format!("team binding rotation failed: {error}")),
                        },
                    );
                    return;
                }
            }
        }

        if let Err(error) = session_store.lock().await.mark_compacted(
            &old_session_id,
            &new_session_id,
            summary_preview.clone(),
        ) {
            if let Some(context) = team_context.as_ref() {
                let rollback_refs_result = {
                    let state = self.state.lock().await;
                    agent_team_validation_refs(&state, "agent_compact_rollback").await
                };
                match rollback_refs_result {
                    Ok(rollback_refs) => {
                        match team_registry
                            .rotate_member_agent(
                                context.team_member_id.clone(),
                                new_agent_id.clone(),
                                agent_id.clone(),
                                new_session_id.clone(),
                                old_session_id.clone(),
                                rollback_refs,
                            )
                            .await
                        {
                            Ok(events) => {
                                let mut state = self.state.lock().await;
                                fan_out_team_registry_events(&mut state, events).await;
                            }
                            Err(rollback_error) => {
                                tracing::error!(
                                    member_id = %context.team_member_id,
                                    error = %rollback_error,
                                    "failed to roll back team binding after session compaction metadata failure"
                                );
                            }
                        }
                    }
                    Err(rollback_error) => {
                        tracing::error!(
                            member_id = %context.team_member_id,
                            error = %rollback_error,
                            "failed to validate team binding rollback after session compaction metadata failure"
                        );
                    }
                }
            }
            self.close_agent(&new_agent_id).await;
            let _ = agent_handle.release_compaction().await;
            send_agent_compact_notify(
                &stream,
                AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: agent_id,
                    old_session_id: Some(old_session_id),
                    new_agent_id: Some(new_agent_id),
                    new_session_id: Some(new_session_id),
                    summary_preview: Some(summary_preview),
                    message: Some(format!("session compaction metadata failed: {error}")),
                },
            );
            return;
        }

        self.fan_out_session_lists().await;
        send_agent_compact_notify(
            &stream,
            AgentCompactNotifyPayload {
                status: AgentCompactStatus::Completed,
                old_agent_id: agent_id.clone(),
                old_session_id: Some(old_session_id),
                new_agent_id: Some(new_agent_id),
                new_session_id: Some(new_session_id),
                summary_preview: Some(summary_preview),
                message: None,
            },
        );
        if !self.close_agent(&agent_id).await {
            tracing::warn!(
                agent_id = %agent_id,
                "old agent was already closed after successful compaction"
            );
        }
    }

    async fn spawn_agent_with_origin(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
    ) -> AppResult<AgentId> {
        self.spawn_agent_with_origin_and_config(payload, origin, None, None)
            .await
    }

    async fn spawn_agent_with_origin_and_config(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
        resolved_spawn_config_override: Option<ResolvedSpawnConfig>,
        workflow: Option<AgentWorkflowMetadata>,
    ) -> AppResult<AgentId> {
        self.spawn_agent_with_origin_config_and_team(
            payload,
            origin,
            resolved_spawn_config_override,
            None,
            workflow,
            None,
        )
        .await
    }

    async fn spawn_agent_with_origin_config_and_team(
        &self,
        payload: SpawnAgentPayload,
        origin: AgentOrigin,
        resolved_spawn_config_override: Option<ResolvedSpawnConfig>,
        team_context: Option<TeamSpawnContext>,
        workflow: Option<AgentWorkflowMetadata>,
        operation_terminal_claim: Option<SpawnOperationTerminalClaim>,
    ) -> AppResult<AgentId> {
        tracing::info!(
            parent_agent_id = ?payload.parent_agent_id,
            project_id = ?payload.project_id,
            requested_name = ?payload.name,
            "host spawn_agent requested"
        );
        let (
            session_store,
            project_store,
            settings_store,
            custom_agent_store,
            mcp_server_store,
            steering_store,
            skill_store,
            use_mock_backend,
            debug_mcp,
            agent_control_mcp,
            config_mcp,
            capacity_tx,
            removing_projects,
            parent_session_id,
            antigravity_conversations_dir,
        ) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_store),
                Arc::clone(&state.project_store),
                Arc::clone(&state.settings_store),
                Arc::clone(&state.custom_agent_store),
                Arc::clone(&state.mcp_server_store),
                Arc::clone(&state.steering_store),
                Arc::clone(&state.skill_store),
                state.use_mock_backend,
                state.debug_mcp.clone(),
                state.agent_control_mcp.clone(),
                state.config_mcp.clone(),
                state.capacity_tx.clone(),
                state.removing_projects.clone(),
                payload.parent_agent_id.as_ref().map(|agent_id| {
                    if let Some(session_id) = state.agent_sessions.get(agent_id).cloned() {
                        Ok(session_id)
                    } else if let Some(handle) = state.registry.agent_handle(agent_id) {
                        handle.snapshot().session_id.ok_or_else(|| {
                            AgentStartupFailure::internal(format!(
                                "fork parent_agent_id {} has no known session_id",
                                agent_id
                            ))
                        })
                    } else {
                        Err(AgentStartupFailure::internal(format!(
                            "fork parent_agent_id {} is not running",
                            agent_id
                        )))
                    }
                }),
                state.antigravity_conversations_dir.clone(),
            )
        };
        let (parent_session_id, parent_session_lookup_failure) = match parent_session_id {
            Some(Ok(session_id)) => (Some(session_id), None),
            Some(Err(err)) => (None, Some(err)),
            None => (None, None),
        };
        let host_settings = settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for spawn: {err}"));

        let mut generated_name_request = None;
        let request = match payload.params {
            SpawnAgentParams::New {
                workspace_roots,
                prompt,
                images,
                backend_kind,
                launch_profile_id,
                cost_hint,
                access_mode,
                session_settings,
            } => {
                let (session_settings, session_settings_source) = self
                    .resolve_launch_profile_session_settings(
                        backend_kind,
                        launch_profile_id.as_ref(),
                        session_settings,
                    )
                    .await?;
                let (project_id, missing_project_failure) = match payload.project_id.clone() {
                    Some(project_id) => {
                        if removing_projects.contains(&project_id) {
                            (
                                None,
                                Some(format!(
                                    "cannot spawn agent in workbench {} because it is being removed",
                                    project_id
                                )),
                            )
                        } else if project_store.lock().await.get(&project_id).is_some() {
                            (Some(project_id), None)
                        } else {
                            (
                                None,
                                Some(format!(
                                    "cannot spawn agent in missing project {}",
                                    project_id
                                )),
                            )
                        }
                    }
                    None => (None, None),
                };
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                    &config_mcp,
                    payload.custom_agent_id.as_ref(),
                );
                let requested_custom_agent_id = payload.custom_agent_id.clone();
                let (
                    effective_custom_agent_id,
                    mut resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = if let Some(err) = missing_project_failure {
                    (
                        requested_custom_agent_id,
                        ResolvedSpawnConfig::default(),
                        None,
                        Some(AgentStartupFailure::backend_failed(err)),
                    )
                } else if let Some(resolved) = resolved_spawn_config_override.clone() {
                    (None, resolved, None, None)
                } else {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind,
                        project_id: project_id.as_ref(),
                        custom_agent_id: requested_custom_agent_id.as_ref(),
                        built_in_mcp_servers: &startup_mcp_servers,
                        custom_agent_store: &custom_agents,
                        mcp_server_store: &mcp_servers,
                        steering_store: &steering,
                        skill_store: &skills,
                    }) {
                        Ok(resolved) => (requested_custom_agent_id, resolved, None, None),
                        Err(err) => (
                            requested_custom_agent_id,
                            ResolvedSpawnConfig::default(),
                            None,
                            Some(AgentStartupFailure::internal(err)),
                        ),
                    }
                };
                if resolved_spawn_config_override.is_none() {
                    resolved_spawn_config.access_mode = access_mode;
                }
                let startup_mcp_servers =
                    protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
                let (session_settings_schema, schema_failure) =
                    match self.resolve_session_schema_for_spawn(backend_kind).await {
                        Ok(schema) => (schema, None),
                        Err(failure) => (None, Some(failure)),
                    };
                let settings_failure = session_settings.as_ref().and_then(|settings| {
                    session_settings_startup_failure(
                        backend_kind,
                        session_settings_schema.as_ref(),
                        settings,
                        session_settings_source,
                    )
                });
                let startup_failure = startup_failure.or(schema_failure).or(settings_failure);
                let (resolved_name, initial_alias) = match payload.name.clone() {
                    Some(name) => (
                        name.clone(),
                        Some(InitialAgentAlias {
                            name,
                            persistence: InitialAgentAliasPersistence::User,
                        }),
                    ),
                    None => {
                        let generated = derive_agent_name(&prompt);
                        if startup_failure.is_none()
                            && host_settings
                                .background_agent_features
                                .auto_generate_agent_names
                        {
                            generated_name_request = Some(GenerateAgentNameRequest {
                                backend_kind,
                                prompt: prompt.clone(),
                                use_mock_backend,
                                capacity_tx: capacity_tx.clone(),
                            });
                        }
                        (
                            generated.clone(),
                            Some(InitialAgentAlias {
                                name: generated,
                                persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
                            }),
                        )
                    }
                };
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin,
                    custom_agent_id: effective_custom_agent_id,
                    team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                    team_member_id: team_context
                        .as_ref()
                        .map(|context| context.team_member_id.clone()),
                    workflow: workflow.clone(),
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id,
                    project_id,
                    backend_kind,
                    launch_profile_id,
                    workspace_roots,
                    initial_input: Some(protocol::SendMessagePayload {
                        message: prompt,
                        images,
                        origin: None,
                        tool_response: None,
                    }),
                    cost_hint,
                    session_settings,
                    session_settings_schema,
                    backend_config: resolve_backend_config_for_spawn(&host_settings, backend_kind),
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: None,
                    fork_from_session_id: None,
                    startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
            SpawnAgentParams::Resume { session_id, prompt } => {
                let record = session_store.lock().await.get(&session_id);
                let Some(record) = record else {
                    let resolved_name = payload
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Session {}", session_id));
                    let initial_alias = payload.name.clone().map(|name| InitialAgentAlias {
                        name,
                        persistence: InitialAgentAliasPersistence::User,
                    });
                    return Ok(self
                        .spawn_resolved_agent(ResolvedSpawnRequest {
                            name: resolved_name,
                            origin,
                            custom_agent_id: payload.custom_agent_id.clone(),
                            team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                            team_member_id: team_context
                                .as_ref()
                                .map(|context| context.team_member_id.clone()),
                            workflow: workflow.clone(),
                            parent_agent_id: payload.parent_agent_id,
                            parent_session_id,
                            project_id: payload.project_id.clone(),
                            backend_kind: host_settings
                                .default_backend
                                .or_else(|| host_settings.enabled_backends.first().copied())
                                .unwrap_or(protocol::BackendKind::Claude),
                            launch_profile_id: None,
                            workspace_roots: Vec::new(),
                            initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                                message: prompt,
                                images: None,
                                origin: None,
                                tool_response: None,
                            }),
                            cost_hint: None,
                            session_settings: None,
                            session_settings_schema: None,
                            backend_config: Default::default(),
                            startup_mcp_servers: Vec::new(),
                            resolved_spawn_config: ResolvedSpawnConfig::default(),
                            resume_session_id: Some(session_id.clone()),
                            fork_from_session_id: None,
                            startup_warning: None,
                            startup_failure: Some(AgentStartupFailure::unsupported(format!(
                                "cannot resume missing session {}",
                                session_id
                            ))),
                            initial_alias,
                            use_mock_backend,
                        })
                        .await);
                };
                if !session_record_is_resumable(&record, &antigravity_conversations_dir) {
                    let resolved_name = payload
                        .name
                        .clone()
                        .or_else(|| record.user_alias.clone())
                        .or_else(|| record.alias.clone())
                        .unwrap_or_else(|| format!("Session {}", session_id));
                    let initial_alias = payload.name.clone().map(|name| InitialAgentAlias {
                        name,
                        persistence: InitialAgentAliasPersistence::User,
                    });
                    return Ok(self
                        .spawn_resolved_agent(ResolvedSpawnRequest {
                            name: resolved_name,
                            origin,
                            custom_agent_id: record.custom_agent_id.clone(),
                            team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                            team_member_id: team_context
                                .as_ref()
                                .map(|context| context.team_member_id.clone()),
                            workflow: workflow.clone(),
                            parent_agent_id: payload.parent_agent_id,
                            parent_session_id,
                            project_id: record.project_id.clone(),
                            backend_kind: record.backend_kind,
                            launch_profile_id: record.launch_profile_id.clone(),
                            workspace_roots: record.workspace_roots.clone(),
                            initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                                message: prompt,
                                images: None,
                                origin: None,
                                tool_response: None,
                            }),
                            cost_hint: None,
                            session_settings: None,
                            session_settings_schema: None,
                            backend_config: Default::default(),
                            startup_mcp_servers: Vec::new(),
                            resolved_spawn_config: ResolvedSpawnConfig::default(),
                            resume_session_id: Some(session_id.clone()),
                            fork_from_session_id: None,
                            startup_warning: None,
                            startup_failure: Some(AgentStartupFailure::unsupported(format!(
                                "cannot resume non-resumable session {}",
                                session_id
                            ))),
                            initial_alias,
                            use_mock_backend,
                        })
                        .await);
                }
                if let Some(requested_custom_agent_id) = payload.custom_agent_id.as_ref() {
                    assert_eq!(
                        record.custom_agent_id.as_ref(),
                        Some(requested_custom_agent_id),
                        "resume custom_agent_id {:?} must match stored session custom_agent_id {:?}",
                        requested_custom_agent_id,
                        record.custom_agent_id
                    );
                }
                let requested_project_id = payload.project_id.or(record.project_id.clone());
                let (project_id, missing_project_warning) = match requested_project_id {
                    Some(project_id) => {
                        if removing_projects.contains(&project_id) {
                            let warning = format!(
                                "workbench {} is being removed; resuming without a project",
                                project_id
                            );
                            (None, Some(warning))
                        } else if project_store.lock().await.get(&project_id).is_some() {
                            (Some(project_id), None)
                        } else {
                            let warning = format!(
                                "project {} was deleted; resuming without a project",
                                project_id
                            );
                            (None, Some(warning))
                        }
                    }
                    None => (None, None),
                };
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &record.workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                    &config_mcp,
                    record.custom_agent_id.as_ref(),
                );
                let (
                    effective_custom_agent_id,
                    resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = if let Some(stored_custom_agent_id) = record.custom_agent_id.as_ref() {
                    if custom_agent_store
                        .lock()
                        .await
                        .get(stored_custom_agent_id)
                        .is_none()
                    {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        (
                            None,
                            resolve_spawn_config(ResolveSpawnConfigRequest {
                                backend_kind: record.backend_kind,
                                project_id: project_id.as_ref(),
                                custom_agent_id: None,
                                built_in_mcp_servers: &startup_mcp_servers,
                                custom_agent_store: &custom_agents,
                                mcp_server_store: &mcp_servers,
                                steering_store: &steering,
                                skill_store: &skills,
                            })
                            .unwrap_or_else(|err| {
                                panic!(
                                    "failed to resolve resume customization after deleted custom agent {}: {err}",
                                    stored_custom_agent_id
                                )
                            }),
                            Some(format!(
                                "custom agent {} was deleted; resuming without custom agent configuration",
                                stored_custom_agent_id
                            )),
                            None,
                        )
                    } else {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        match resolve_spawn_config(ResolveSpawnConfigRequest {
                            backend_kind: record.backend_kind,
                            project_id: project_id.as_ref(),
                            custom_agent_id: Some(stored_custom_agent_id),
                            built_in_mcp_servers: &startup_mcp_servers,
                            custom_agent_store: &custom_agents,
                            mcp_server_store: &mcp_servers,
                            steering_store: &steering,
                            skill_store: &skills,
                        }) {
                            Ok(resolved) => {
                                (Some(stored_custom_agent_id.clone()), resolved, None, None)
                            }
                            Err(err) => (
                                Some(stored_custom_agent_id.clone()),
                                ResolvedSpawnConfig::default(),
                                None,
                                Some(AgentStartupFailure::internal(err)),
                            ),
                        }
                    }
                } else {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind: record.backend_kind,
                        project_id: project_id.as_ref(),
                        custom_agent_id: None,
                        built_in_mcp_servers: &startup_mcp_servers,
                        custom_agent_store: &custom_agents,
                        mcp_server_store: &mcp_servers,
                        steering_store: &steering,
                        skill_store: &skills,
                    }) {
                        Ok(resolved) => (None, resolved, None, None),
                        Err(err) => (
                            None,
                            ResolvedSpawnConfig::default(),
                            None,
                            Some(AgentStartupFailure::internal(err)),
                        ),
                    }
                };
                let startup_mcp_servers =
                    protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
                let (session_settings_schema, schema_failure) = match self
                    .resolve_session_schema_for_spawn(record.backend_kind)
                    .await
                {
                    Ok(schema) => (schema, None),
                    Err(failure) => (None, Some(failure)),
                };
                let (resolved_name, initial_alias) = match payload.name.clone() {
                    Some(name) => (
                        name.clone(),
                        Some(InitialAgentAlias {
                            name,
                            persistence: InitialAgentAliasPersistence::User,
                        }),
                    ),
                    None => {
                        let effective = record
                            .user_alias
                            .clone()
                            .or(record.alias.clone())
                            .unwrap_or_else(|| {
                                panic!(
                                    "cannot resume session {} without a stored effective name",
                                    session_id
                                )
                            });
                        (effective, None)
                    }
                };
                let (sanitized_settings, settings_failure) = sanitize_stored_session_settings(
                    record.backend_kind,
                    session_settings_schema.as_ref(),
                    record.session_settings.clone(),
                );
                let combined_startup_warning = match (startup_warning, missing_project_warning) {
                    (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin,
                    custom_agent_id: effective_custom_agent_id,
                    team_id: team_context.as_ref().map(|context| context.team_id.clone()),
                    team_member_id: team_context
                        .as_ref()
                        .map(|context| context.team_member_id.clone()),
                    workflow: workflow.clone(),
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id,
                    project_id,
                    backend_kind: record.backend_kind,
                    launch_profile_id: record.launch_profile_id.clone(),
                    workspace_roots: record.workspace_roots,
                    initial_input: prompt.map(|prompt| protocol::SendMessagePayload {
                        message: prompt,
                        images: None,
                        origin: None,
                        tool_response: None,
                    }),
                    cost_hint: None,
                    session_settings: sanitized_settings,
                    session_settings_schema,
                    backend_config: resolve_backend_config_for_spawn(
                        &host_settings,
                        record.backend_kind,
                    ),
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: Some(session_id),
                    fork_from_session_id: None,
                    startup_warning: combined_startup_warning,
                    startup_failure: startup_failure.or(schema_failure).or(settings_failure),
                    initial_alias,
                    use_mock_backend,
                }
            }
            SpawnAgentParams::Fork {
                from_session_id,
                prompt,
                images,
                access_mode,
            } => {
                assert!(
                    payload.parent_agent_id.is_some(),
                    "fork spawn requires parent_agent_id"
                );
                let record = session_store.lock().await.get(&from_session_id);
                let Some(record) = record else {
                    let resolved_name = payload.name.clone().unwrap_or_else(|| {
                        let prompt_name = derive_agent_name(&prompt);
                        format!("BTW: {prompt_name}")
                    });
                    let initial_alias = Some(InitialAgentAlias {
                        name: resolved_name.clone(),
                        persistence: if payload.name.is_some() {
                            InitialAgentAliasPersistence::User
                        } else {
                            InitialAgentAliasPersistence::GeneratedIfNoUserAlias
                        },
                    });
                    let resolved_spawn_config = ResolvedSpawnConfig {
                        access_mode: access_mode.unwrap_or(protocol::BackendAccessMode::ReadOnly),
                        ..Default::default()
                    };
                    return Ok(self
                        .spawn_resolved_agent(ResolvedSpawnRequest {
                            name: resolved_name,
                            origin: AgentOrigin::SideQuestion,
                            custom_agent_id: payload.custom_agent_id,
                            team_id: None,
                            team_member_id: None,
                            workflow: None,
                            parent_agent_id: payload.parent_agent_id,
                            parent_session_id: Some(from_session_id.clone()),
                            project_id: payload.project_id,
                            backend_kind: protocol::BackendKind::Claude,
                            launch_profile_id: None,
                            workspace_roots: Vec::new(),
                            initial_input: Some(protocol::SendMessagePayload {
                                message: prompt,
                                images,
                                origin: None,
                                tool_response: None,
                            }),
                            cost_hint: None,
                            session_settings: None,
                            session_settings_schema: None,
                            backend_config: Default::default(),
                            startup_mcp_servers: Vec::new(),
                            resolved_spawn_config,
                            resume_session_id: None,
                            fork_from_session_id: None,
                            startup_warning: None,
                            startup_failure: Some(AgentStartupFailure::internal(format!(
                                "cannot fork missing session {}",
                                from_session_id
                            ))),
                            initial_alias,
                            use_mock_backend,
                        })
                        .await);
                };
                let parent_agent_mismatch_failure = match parent_session_id.as_ref() {
                    Some(session_id) if session_id != &from_session_id => {
                        Some(AgentStartupFailure::internal(format!(
                            "fork parent_agent_id maps to session {}, not from_session_id {}",
                            session_id, from_session_id
                        )))
                    }
                    Some(_) => None,
                    None => parent_session_lookup_failure.clone(),
                };
                tracing::warn!(
                    from_session_id = %from_session_id,
                    parent_agent_id = ?payload.parent_agent_id,
                    parent_lookup_failed = parent_agent_mismatch_failure.is_some(),
                    "diagnostic: side-question fork loaded source session and resolved parent"
                );
                if let Some(requested_custom_agent_id) = payload.custom_agent_id.as_ref() {
                    assert_eq!(
                        record.custom_agent_id.as_ref(),
                        Some(requested_custom_agent_id),
                        "fork custom_agent_id {:?} must match stored session custom_agent_id {:?}",
                        requested_custom_agent_id,
                        record.custom_agent_id
                    );
                }

                let requested_project_id = record.project_id.clone();
                let (project_id, missing_project_warning) = match requested_project_id {
                    Some(project_id) => {
                        if removing_projects.contains(&project_id) {
                            let warning = format!(
                                "workbench {} is being removed; forking without a project",
                                project_id
                            );
                            (None, Some(warning))
                        } else if project_store.lock().await.get(&project_id).is_some() {
                            (Some(project_id), None)
                        } else {
                            let warning = format!(
                                "project {} was deleted; forking without a project",
                                project_id
                            );
                            (None, Some(warning))
                        }
                    }
                    None => (None, None),
                };
                let workspace_roots = record.workspace_roots.clone();
                let backend_kind = record.backend_kind;
                let startup_mcp_servers = startup_mcp_servers_for_settings(
                    &host_settings,
                    &workspace_roots,
                    &debug_mcp,
                    &agent_control_mcp,
                    &config_mcp,
                    record.custom_agent_id.as_ref(),
                );
                let (
                    effective_custom_agent_id,
                    mut resolved_spawn_config,
                    startup_warning,
                    startup_failure,
                ) = if let Some(stored_custom_agent_id) = record.custom_agent_id.as_ref() {
                    if custom_agent_store
                        .lock()
                        .await
                        .get(stored_custom_agent_id)
                        .is_none()
                    {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        (
                            None,
                            resolve_spawn_config(ResolveSpawnConfigRequest {
                                backend_kind,
                                project_id: project_id.as_ref(),
                                custom_agent_id: None,
                                built_in_mcp_servers: &startup_mcp_servers,
                                custom_agent_store: &custom_agents,
                                mcp_server_store: &mcp_servers,
                                steering_store: &steering,
                                skill_store: &skills,
                            })
                            .unwrap_or_else(|err| {
                                panic!(
                                    "failed to resolve fork customization after deleted custom agent {}: {err}",
                                    stored_custom_agent_id
                                )
                            }),
                            Some(format!(
                                "custom agent {} was deleted; forking without custom agent configuration",
                                stored_custom_agent_id
                            )),
                            None,
                        )
                    } else {
                        let custom_agents = custom_agent_store.lock().await;
                        let mcp_servers = mcp_server_store.lock().await;
                        let steering = steering_store.lock().await;
                        let skills = skill_store.lock().await;
                        match resolve_spawn_config(ResolveSpawnConfigRequest {
                            backend_kind,
                            project_id: project_id.as_ref(),
                            custom_agent_id: Some(stored_custom_agent_id),
                            built_in_mcp_servers: &startup_mcp_servers,
                            custom_agent_store: &custom_agents,
                            mcp_server_store: &mcp_servers,
                            steering_store: &steering,
                            skill_store: &skills,
                        }) {
                            Ok(resolved) => {
                                (Some(stored_custom_agent_id.clone()), resolved, None, None)
                            }
                            Err(err) => (
                                Some(stored_custom_agent_id.clone()),
                                ResolvedSpawnConfig::default(),
                                None,
                                Some(AgentStartupFailure::internal(err)),
                            ),
                        }
                    }
                } else if let Some(resolved) = resolved_spawn_config_override.clone() {
                    (None, resolved, None, None)
                } else {
                    let custom_agents = custom_agent_store.lock().await;
                    let mcp_servers = mcp_server_store.lock().await;
                    let steering = steering_store.lock().await;
                    let skills = skill_store.lock().await;
                    match resolve_spawn_config(ResolveSpawnConfigRequest {
                        backend_kind,
                        project_id: project_id.as_ref(),
                        custom_agent_id: None,
                        built_in_mcp_servers: &startup_mcp_servers,
                        custom_agent_store: &custom_agents,
                        mcp_server_store: &mcp_servers,
                        steering_store: &steering,
                        skill_store: &skills,
                    }) {
                        Ok(resolved) => (None, resolved, None, None),
                        Err(err) => (
                            None,
                            ResolvedSpawnConfig::default(),
                            None,
                            Some(AgentStartupFailure::internal(err)),
                        ),
                    }
                };
                resolved_spawn_config.access_mode =
                    access_mode.unwrap_or(protocol::BackendAccessMode::ReadOnly);
                let startup_mcp_servers =
                    protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
                let (session_settings_schema, schema_failure) =
                    if parent_agent_mismatch_failure.is_some() {
                        let state = self.state.lock().await;
                        (session_schema_for_backend(&state, backend_kind), None)
                    } else {
                        match self.resolve_session_schema_for_spawn(backend_kind).await {
                            Ok(schema) => (schema, None),
                            Err(failure) => (None, Some(failure)),
                        }
                    };
                let (sanitized_settings, settings_failure) = sanitize_stored_session_settings(
                    backend_kind,
                    session_settings_schema.as_ref(),
                    record.session_settings.clone(),
                );
                let backend_support_failure = (!use_mock_backend
                    && !matches!(
                        backend_kind,
                        protocol::BackendKind::Claude | protocol::BackendKind::Codex
                    ))
                .then(|| {
                    AgentStartupFailure::unsupported(
                        crate::backend::backend_fork_unsupported_message(backend_kind),
                    )
                });
                let non_resumable_failure =
                    (!session_record_is_resumable(&record, &antigravity_conversations_dir)).then(
                        || {
                            AgentStartupFailure::unsupported(format!(
                                "cannot fork non-resumable session {}",
                                from_session_id
                            ))
                        },
                    );
                let startup_failure = startup_failure
                    .or(parent_agent_mismatch_failure)
                    .or(non_resumable_failure)
                    .or(backend_support_failure)
                    .or(schema_failure)
                    .or(settings_failure);
                let combined_startup_warning = match (startup_warning, missing_project_warning) {
                    (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                let (resolved_name, initial_alias) = match payload.name.clone() {
                    Some(name) => (
                        name.clone(),
                        Some(InitialAgentAlias {
                            name,
                            persistence: InitialAgentAliasPersistence::User,
                        }),
                    ),
                    None => {
                        let provisional = format!("BTW: {}", derive_agent_name(&prompt));
                        (
                            provisional.clone(),
                            Some(InitialAgentAlias {
                                name: provisional,
                                persistence: InitialAgentAliasPersistence::GeneratedIfNoUserAlias,
                            }),
                        )
                    }
                };
                ResolvedSpawnRequest {
                    name: resolved_name,
                    origin: AgentOrigin::SideQuestion,
                    custom_agent_id: effective_custom_agent_id,
                    team_id: None,
                    team_member_id: None,
                    workflow: None,
                    parent_agent_id: payload.parent_agent_id,
                    parent_session_id: Some(from_session_id.clone()),
                    project_id,
                    backend_kind,
                    launch_profile_id: None,
                    workspace_roots,
                    initial_input: Some(protocol::SendMessagePayload {
                        message: prompt,
                        images,
                        origin: None,
                        tool_response: None,
                    }),
                    cost_hint: None,
                    session_settings: sanitized_settings,
                    session_settings_schema,
                    backend_config: resolve_backend_config_for_spawn(
                        &host_settings,
                        record.backend_kind,
                    ),
                    startup_mcp_servers,
                    resolved_spawn_config,
                    resume_session_id: None,
                    fork_from_session_id: Some(from_session_id),
                    startup_warning: combined_startup_warning,
                    startup_failure,
                    initial_alias,
                    use_mock_backend,
                }
            }
        };

        let request = self.apply_complexity_tier_settings(request).await;
        let diagnose_side_question_fanout = matches!(&request.origin, AgentOrigin::SideQuestion);
        tracing::info!(
            backend_kind = ?request.backend_kind,
            workspace_roots = ?request.workspace_roots,
            startup_mcp_servers = request.startup_mcp_servers.len(),
            resume_session_id = ?request.resume_session_id,
            fork_from_session_id = ?request.fork_from_session_id,
            "host spawn_agent resolved request"
        );

        let (start, agent_handle, startup_rx, agent_visibility) = {
            let mut state = self.state.lock().await;
            let sub_agent_spawn_tx = state.sub_agent_spawn_tx.clone();
            let capacity_tx = state.capacity_tx.clone();
            let review_registry = state.review_registry.clone();
            let agent_control_mcp = state.agent_control_mcp.clone();
            let antigravity_conversations_dir = state.antigravity_conversations_dir.clone();
            let spawned = state.registry.spawn(
                request,
                &agent_control_mcp,
                crate::agent::AgentActorRuntimeResources {
                    session_store: Arc::clone(&session_store),
                    host_sub_agent_spawn_tx: sub_agent_spawn_tx,
                    capacity_tx,
                    review_registry,
                    antigravity_conversations_dir,
                },
            );
            (
                spawned.start,
                spawned.handle,
                spawned.startup_rx,
                state.agent_visibility.clone(),
            )
        };

        let agent_id = start.agent_id.clone();
        let visibility = SpawnVisibility::new(agent_id.clone(), agent_visibility);
        let mut visibility_guard = SpawnVisibilityGuard::new(visibility.clone());
        let session_registration_publish = self.schedule_agent_session_registration(
            agent_id.clone(),
            startup_rx,
            visibility.clone(),
        );
        #[cfg(test)]
        wait_for_spawn_session_registration_test_hook(self).await;

        #[cfg(test)]
        wait_before_startup_failure_fanout_test_hook(self).await;
        let fanout_started = visibility.begin_fanout();
        #[cfg(test)]
        notify_startup_failure_fanout_claimed_test_hook(self);
        if !fanout_started {
            return Ok(agent_id);
        }
        let mut host_streams = {
            let mut state = self.state.lock().await;
            let activity_summary =
                initial_agent_activity_summary_state(&mut state, &start.agent_id);
            let host_streams = state
                .host_streams
                .iter_mut()
                .filter_map(|(path, subscriber)| {
                    prepare_new_agent_fanout_for_subscriber(
                        subscriber,
                        &start,
                        &agent_handle,
                        activity_summary.clone(),
                    )
                    .map(
                        |(stream, attach_eagerly, instance_stream, activity_summary)| {
                            (
                                path.clone(),
                                stream,
                                attach_eagerly,
                                instance_stream,
                                activity_summary,
                            )
                        },
                    )
                })
                .collect::<Vec<_>>();
            if diagnose_side_question_fanout {
                tracing::warn!(
                    agent_id = %start.agent_id,
                    parent_agent_id = ?start.parent_agent_id,
                    registered = state.registry.agent_handle(&start.agent_id).is_some(),
                    prepared_host_fanouts = host_streams.len(),
                    total_host_subscribers = state.host_streams.len(),
                    "diagnostic: side-question agent registered and host fanout prepared"
                );
            }
            host_streams
        };
        host_streams.sort_by(|left, right| left.0.0.cmp(&right.0.0));

        let mut dead_paths = Vec::new();
        let mut deferred_attachments = Vec::new();
        let mut publication_claimed = false;
        #[cfg(test)]
        let mut successful_fanouts = 0_usize;
        for (path, stream, attach_eagerly, instance_stream, activity_summary) in host_streams {
            if !visibility.may_emit_new_agent() {
                break;
            }
            if diagnose_side_question_fanout {
                tracing::warn!(
                    agent_id = %start.agent_id,
                    host_stream = %path,
                    agent_stream = %instance_stream,
                    attach_eagerly,
                    "diagnostic: attempting side-question NewAgent fanout"
                );
            }
            match emit_new_agent_for_stream(
                &start,
                &agent_handle,
                &stream,
                instance_stream,
                attach_eagerly,
                activity_summary,
            ) {
                Ok(attachment) => {
                    let continue_fanout = visibility.record_new_agent_delivery(path.clone());
                    if let Some(terminal_claim) = operation_terminal_claim.as_ref() {
                        publication_claimed |= terminal_claim.claim_success_at_publication();
                    }
                    if let Some(attachment) = attachment {
                        deferred_attachments.push(attachment);
                    }
                    #[cfg(test)]
                    {
                        successful_fanouts += 1;
                    }
                    if diagnose_side_question_fanout {
                        tracing::warn!(
                            agent_id = %start.agent_id,
                            host_stream = %path,
                            attach_eagerly,
                            "diagnostic: side-question NewAgent fanout synchronously enqueued"
                        );
                    }
                    if !continue_fanout {
                        break;
                    }
                }
                Err(error) => {
                    if diagnose_side_question_fanout {
                        tracing::warn!(
                            agent_id = %start.agent_id,
                            host_stream = %path,
                            error = ?error,
                            "diagnostic: side-question NewAgent fanout failed"
                        );
                    }
                    dead_paths.push(path);
                }
            }
        }
        if let Some(request) = generated_name_request {
            self.schedule_generated_agent_name(agent_handle.clone(), request);
        }
        for attachment in deferred_attachments {
            self.attach_deferred_agent_stream(attachment).await;
        }
        if let Some(terminal_claim) = operation_terminal_claim.as_ref() {
            terminal_claim
                .wait_after_success_publication(publication_claimed)
                .await;
        }
        #[cfg(test)]
        for _ in 0..successful_fanouts {
            wait_after_spawn_new_agent_fanout_test_hook(self).await;
        }
        if !dead_paths.is_empty() {
            let mut state = self.state.lock().await;
            for path in dead_paths {
                state.host_streams.remove(&path);
                state.agent_visibility.remove_host_stream(&path);
            }
        }
        if visibility.finish_new_agent_fanout() {
            return Ok(agent_id);
        }
        #[cfg(test)]
        wait_for_spawn_visible_before_publication_test_hook(self).await;
        {
            let mut state = self.state.lock().await;
            fan_out_current_agents_view_preferences(&mut state).await;
        }

        if let Some(project_id) = start.project_id.clone()
            && let Err(error) = self
                .warm_code_intel_project(project_id.clone(), "agent_start")
                .await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to warm code intelligence after agent launch"
            );
        }

        session_registration_publish.publish();
        visibility_guard.disarm();
        tracing::info!(
            agent_id = %agent_id,
            backend_kind = ?start.backend_kind,
            name = %start.name,
            "host spawn_agent completed"
        );

        Ok(agent_id)
    }

    async fn resolve_launch_profile_session_settings(
        &self,
        backend_kind: BackendKind,
        launch_profile_id: Option<&LaunchProfileId>,
        explicit_session_settings: Option<protocol::SessionSettingsValues>,
    ) -> AppResult<(Option<protocol::SessionSettingsValues>, &'static str)> {
        let Some(launch_profile_id) = launch_profile_id else {
            return Ok((explicit_session_settings, "supplied"));
        };
        let profile = self
            .resolve_launch_profile(launch_profile_id)
            .await
            .map_err(|error| AppError::invalid("spawn_agent", error))?;
        if profile.backend_kind != backend_kind {
            return Err(AppError::conflict(
                "spawn_agent",
                format!(
                    "launch_profile_id {launch_profile_id} targets {:?}, but backend_kind is {:?}",
                    profile.backend_kind, backend_kind
                ),
            ));
        }
        let mut merged = profile.session_settings;
        let source = if let Some(explicit) = explicit_session_settings.as_ref() {
            apply_session_settings_update(&mut merged, explicit);
            "launch profile merged with supplied"
        } else {
            "launch profile"
        };
        Ok(((!merged.0.is_empty()).then_some(merged), source))
    }

    /// Applies the host-level "task complexity tiers" setting to a spawn request.
    ///
    /// When tiers are disabled (the default), the cost hint is dropped so every
    /// spawn uses the backend's own defaults. When enabled, a Low/High hint
    /// resolves through the user's per-backend tier config when one exists
    /// (explicit session settings still win); backends without a config fall
    /// back to their built-in mapping. Medium is a legacy no-op.
    async fn apply_complexity_tier_settings(
        &self,
        mut request: ResolvedSpawnRequest,
    ) -> ResolvedSpawnRequest {
        let Some(hint) = request.cost_hint else {
            return request;
        };
        let settings_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.settings_store)
        };
        let settings = match settings_store.lock().await.get() {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(%error, "failed to read host settings; ignoring spawn cost hint");
                request.cost_hint = None;
                return request;
            }
        };
        if !settings.complexity_tiers_enabled {
            request.cost_hint = None;
            return request;
        }
        let tier_values = match (
            hint,
            settings.backend_tier_configs.get(&request.backend_kind),
        ) {
            (protocol::SpawnCostHint::Medium, _) => {
                request.cost_hint = None;
                return request;
            }
            (_, None) if request.backend_kind == protocol::BackendKind::Codex => {
                let Some(schema) = request.session_settings_schema.as_ref() else {
                    request.startup_failure = request.startup_failure.or_else(|| {
                        Some(AgentStartupFailure::backend_failed(
                            "Codex session settings schema unavailable; cannot resolve complexity tier",
                        ))
                    });
                    request.cost_hint = None;
                    return request;
                };
                let selected_values = request.session_settings.clone().unwrap_or_default();
                let config = match crate::backend::codex::codex_tier_config_from_schema(
                    schema,
                    &selected_values,
                ) {
                    Ok(config) => config,
                    Err(error) => {
                        request.startup_failure = request.startup_failure.or_else(|| {
                            Some(AgentStartupFailure::backend_failed(format!(
                                "failed to resolve Codex complexity tier from model metadata: {error}"
                            )))
                        });
                        request.cost_hint = None;
                        return request;
                    }
                };
                tier_values_for_hint(hint, &config)
            }
            // No user config for this backend: the backend's built-in
            // tier mapping applies via the cost hint as before.
            (_, None) => {
                if backend_has_dynamic_session_schema(request.backend_kind)
                    && request.session_settings_schema.is_none()
                {
                    let builtin = crate::backend::builtin_tier_config(request.backend_kind);
                    let tier_values = tier_values_for_hint(hint, &builtin);
                    if !tier_values.0.is_empty() {
                        request.startup_failure = request.startup_failure.or_else(|| {
                            session_settings_startup_failure(
                                request.backend_kind,
                                None,
                                &tier_values,
                                "complexity-tier",
                            )
                        });
                        request.session_settings = None;
                        request.cost_hint = None;
                    }
                }
                return request;
            }
            (protocol::SpawnCostHint::Low, Some(config)) => config.low.clone(),
            (protocol::SpawnCostHint::High, Some(config)) => config.high.clone(),
        };
        if backend_has_dynamic_session_schema(request.backend_kind)
            && request.session_settings_schema.is_none()
        {
            let mut merged = tier_values;
            if let Some(explicit) = request.session_settings.take() {
                apply_session_settings_update(&mut merged, &explicit);
            }
            if !merged.0.is_empty() {
                request.startup_failure = request.startup_failure.or_else(|| {
                    session_settings_startup_failure(
                        request.backend_kind,
                        None,
                        &merged,
                        "complexity-tier",
                    )
                });
            }
            request.session_settings = None;
            request.cost_hint = None;
            return request;
        }
        let mut merged = tier_values;
        if let Some(explicit) = request.session_settings.take() {
            apply_session_settings_update(&mut merged, &explicit);
        }
        if let Some(schema) = request.session_settings_schema.as_ref()
            && let Err(error) = validate_session_settings_values(schema, &merged)
        {
            request.startup_failure = request.startup_failure.or_else(|| {
                Some(AgentStartupFailure::internal(format!(
                    "invalid complexity-tier session settings for backend {:?}: {error}",
                    request.backend_kind
                )))
            });
        }
        request.session_settings = (!merged.0.is_empty()).then_some(merged);
        request.cost_hint = None;
        request
    }

    async fn spawn_resolved_agent(&self, request: ResolvedSpawnRequest) -> AgentId {
        let request = self.apply_complexity_tier_settings(request).await;
        tracing::info!(
            backend_kind = ?request.backend_kind,
            workspace_roots = ?request.workspace_roots,
            startup_mcp_servers = request.startup_mcp_servers.len(),
            resume_session_id = ?request.resume_session_id,
            fork_from_session_id = ?request.fork_from_session_id,
            "host spawn_agent resolved request"
        );

        let session_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.session_store)
        };
        let (start, agent_handle, startup_rx, agent_visibility) = {
            let mut state = self.state.lock().await;
            let sub_agent_spawn_tx = state.sub_agent_spawn_tx.clone();
            let capacity_tx = state.capacity_tx.clone();
            let review_registry = state.review_registry.clone();
            let agent_control_mcp = state.agent_control_mcp.clone();
            let antigravity_conversations_dir = state.antigravity_conversations_dir.clone();
            let spawned = state.registry.spawn(
                request,
                &agent_control_mcp,
                crate::agent::AgentActorRuntimeResources {
                    session_store,
                    host_sub_agent_spawn_tx: sub_agent_spawn_tx,
                    capacity_tx,
                    review_registry,
                    antigravity_conversations_dir,
                },
            );
            (
                spawned.start,
                spawned.handle,
                spawned.startup_rx,
                state.agent_visibility.clone(),
            )
        };

        let agent_id = start.agent_id.clone();
        let visibility = SpawnVisibility::new(agent_id.clone(), agent_visibility);
        let mut visibility_guard = SpawnVisibilityGuard::new(visibility.clone());
        let session_registration_publish = self.schedule_agent_session_registration(
            agent_id.clone(),
            startup_rx,
            visibility.clone(),
        );
        #[cfg(test)]
        wait_for_spawn_session_registration_test_hook(self).await;

        #[cfg(test)]
        wait_before_startup_failure_fanout_test_hook(self).await;
        let fanout_started = visibility.begin_fanout();
        #[cfg(test)]
        notify_startup_failure_fanout_claimed_test_hook(self);
        if !fanout_started {
            return agent_id;
        }
        let mut host_streams = {
            let mut state = self.state.lock().await;
            let activity_summary =
                initial_agent_activity_summary_state(&mut state, &start.agent_id);
            state
                .host_streams
                .iter_mut()
                .filter_map(|(path, subscriber)| {
                    prepare_new_agent_fanout_for_subscriber(
                        subscriber,
                        &start,
                        &agent_handle,
                        activity_summary.clone(),
                    )
                    .map(
                        |(stream, attach_eagerly, instance_stream, activity_summary)| {
                            (
                                path.clone(),
                                stream,
                                attach_eagerly,
                                instance_stream,
                                activity_summary,
                            )
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        host_streams.sort_by(|left, right| left.0.0.cmp(&right.0.0));

        let mut dead_paths = Vec::new();
        let mut deferred_attachments = Vec::new();
        #[cfg(test)]
        let mut successful_fanouts = 0_usize;
        for (path, stream, attach_eagerly, instance_stream, activity_summary) in host_streams {
            if !visibility.may_emit_new_agent() {
                break;
            }
            match emit_new_agent_for_stream(
                &start,
                &agent_handle,
                &stream,
                instance_stream,
                attach_eagerly,
                activity_summary,
            ) {
                Ok(attachment) => {
                    let continue_fanout = visibility.record_new_agent_delivery(path.clone());
                    if let Some(attachment) = attachment {
                        deferred_attachments.push(attachment);
                    }
                    #[cfg(test)]
                    {
                        successful_fanouts += 1;
                    }
                    if !continue_fanout {
                        break;
                    }
                }
                Err(_) => dead_paths.push(path),
            }
        }
        for attachment in deferred_attachments {
            self.attach_deferred_agent_stream(attachment).await;
        }
        #[cfg(test)]
        for _ in 0..successful_fanouts {
            wait_after_spawn_new_agent_fanout_test_hook(self).await;
        }
        if !dead_paths.is_empty() {
            let mut state = self.state.lock().await;
            for path in dead_paths {
                state.host_streams.remove(&path);
                state.agent_visibility.remove_host_stream(&path);
            }
        }
        if visibility.finish_new_agent_fanout() {
            return agent_id;
        }
        #[cfg(test)]
        wait_for_spawn_visible_before_publication_test_hook(self).await;

        if let Some(project_id) = start.project_id.clone()
            && let Err(error) = self
                .warm_code_intel_project(project_id.clone(), "agent_start")
                .await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to warm code intelligence after agent launch"
            );
        }

        session_registration_publish.publish();
        visibility_guard.disarm();
        tracing::info!(
            agent_id = %agent_id,
            backend_kind = ?start.backend_kind,
            name = %start.name,
            "host spawn_agent completed"
        );
        agent_id
    }

    pub(crate) async fn create_project(&self, payload: ProjectCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "project_create";
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .create(payload.name, payload.roots)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        let project_id = project.id.clone();
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        if let Err(error) = ensure_project_actor(&mut state, project_id.clone()).await {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to start project actor after project creation"
            );
        }
        let host_paths = state.host_streams.keys().cloned().collect::<Vec<_>>();
        for host_path in host_paths {
            if let Err(error) =
                subscribe_host_to_project(&mut state, &host_path, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to subscribe host to project stream after project creation"
                );
            }
            if let Err(error) =
                emit_review_list_changed_for_project(&mut state, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to emit initial review list after project creation"
                );
            }
        }
        drop(state);
        self.update_workflow_watcher_targets_and_reload("project_create")
            .await?;
        Ok(())
    }

    pub(crate) async fn rename_project(&self, payload: ProjectRenamePayload) -> AppResult<()> {
        const OPERATION: &str = "project_rename";
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .rename(&payload.id, payload.name)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        fan_out_current_agents_view_preferences(&mut state).await;
        Ok(())
    }

    pub(crate) async fn reorder_projects(&self, payload: ProjectReorderPayload) -> AppResult<()> {
        const OPERATION: &str = "project_reorder";
        let mut state = self.state.lock().await;
        let projects = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .reorder(payload.scope, payload.project_ids)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        for project in projects {
            fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        }
        Ok(())
    }

    pub(crate) async fn add_project_root(&self, payload: ProjectAddRootPayload) -> AppResult<()> {
        const OPERATION: &str = "project_add_root";
        let parent_lock = self.workbench_parent_lock(&payload.id).await;
        let _parent_guard = parent_lock.lock().await;
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .add_root(&payload.id, payload.root)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        let project_id = project.id.clone();
        let roots = project.root_paths();
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        if let Some(router) = state.code_intel_routers.get_mut(&project_id) {
            router.retain_roots(&roots);
        }
        let handle = match ensure_project_actor(&mut state, project_id.clone()).await {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "failed to start project actor after adding project root"
                );
                None
            }
        };
        drop(state);
        if let Some(handle) = handle
            && let Err(error) = handle.refresh().await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to refresh project actor after adding project root"
            );
        }
        self.update_workflow_watcher_targets_and_reload("project_add_root")
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_project_root(
        &self,
        payload: ProjectDeleteRootPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_delete_root";
        let parent_lock = self.workbench_parent_lock(&payload.id).await;
        let _parent_guard = parent_lock.lock().await;
        let mut state = self.state.lock().await;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .delete_root(&payload.id, &payload.root)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        let project_id = project.id.clone();
        let roots = project.root_paths();
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Upsert { project }).await;
        if let Some(router) = state.code_intel_routers.get_mut(&project_id) {
            router.retain_roots(&roots);
        }
        let handle = match ensure_project_actor(&mut state, project_id.clone()).await {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "failed to start project actor after deleting project root"
                );
                None
            }
        };
        drop(state);
        if let Some(handle) = handle
            && let Err(error) = handle.refresh().await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to refresh project actor after deleting project root"
            );
        }
        self.update_workflow_watcher_targets_and_reload("project_delete_root")
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_project(&self, payload: ProjectDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "project_delete";
        let parent_lock = self.workbench_parent_lock(&payload.id).await;
        let _parent_guard = parent_lock.lock().await;
        let mut state = self.state.lock().await;
        {
            let project_store = state.project_store.lock().await;
            let Some(project) = project_store.get(&payload.id) else {
                return Err(AppError::not_found(
                    OPERATION,
                    format!("cannot delete missing project {}", payload.id),
                ));
            };
            if project.is_workbench() {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "cannot delete workbench project {} with ProjectDelete; use WorkbenchRemove",
                        payload.id
                    ),
                ));
            }
            if let Some(child) = project_store.list_children(&payload.id).first() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot delete project {} while referenced by workbench {}",
                        payload.id, child
                    ),
                ));
            }
        }
        let deleted_steering_ids = state
            .steering_store
            .lock()
            .await
            .delete_for_project(&payload.id)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let detached_session_ids = state
            .session_store
            .lock()
            .await
            .detach_project(&payload.id)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let team_refs = agent_team_validation_refs(&state, OPERATION).await?;
        let team_events = state
            .team_registry
            .remove_project_refs(payload.id.clone(), team_refs)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        let project = {
            let mut project_store = state.project_store.lock().await;
            project_store
                .delete(&payload.id)
                .map_err(|error| project_store_error(OPERATION, error))?
        };
        cleanup_reviews_for_deleted_project(&state.review_registry, &payload.id).await;
        if let Some(mut router) = state.code_intel_routers.remove(&payload.id) {
            router.shutdown_all();
        }
        if let Some(subscription) = state.project_streams.remove(&payload.id) {
            subscription.task.abort();
        }
        for id in deleted_steering_ids {
            fan_out_steering_notify(&mut state, SteeringNotifyPayload::Delete { id }).await;
        }
        fan_out_team_registry_events(&mut state, team_events).await;
        fan_out_project_notify(&mut state, ProjectNotifyPayload::Delete { project }).await;
        fan_out_current_agents_view_preferences(&mut state).await;
        if !detached_session_ids.is_empty() {
            fan_out_session_lists(&mut state).await;
        }
        drop(state);
        self.update_workflow_watcher_targets_and_reload("project_delete")
            .await?;
        Ok(())
    }

    pub(crate) async fn create_workbench(
        &self,
        payload: WorkbenchCreatePayload,
        base: Option<BaseRevision>,
    ) -> AppResult<CreatedWorkbench> {
        const OPERATION: &str = "workbench_create";
        let parent_lock = self.workbench_parent_lock(&payload.parent_project_id).await;
        let _parent_guard = parent_lock.lock().await;

        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };

        let parent = {
            let project_store = project_store.lock().await;
            project_store
                .get(&payload.parent_project_id)
                .ok_or_else(|| {
                    AppError::not_found(
                        OPERATION,
                        format!("project {} not found", payload.parent_project_id),
                    )
                })?
        };

        let ProjectSource::Standalone {
            roots: parent_roots,
        } = &parent.source
        else {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "cannot create workbench for non-standalone parent project {}",
                    parent.id
                ),
            ));
        };

        if parent_roots.is_empty() {
            return Err(AppError::internal_message(
                OPERATION,
                format!("standalone parent project {} has no roots", parent.id),
                anyhow!("standalone parent project has no roots"),
            ));
        }

        let branch = payload.branch.clone();
        let roots = compute_workbench_roots(parent_roots, &branch)?;
        preflight_workbench_create(&project_store, &parent, &branch, &roots).await?;

        let mut resolved_roots = Vec::with_capacity(roots.len());
        for root in &roots {
            resolved_roots.push(CreatedWorkbenchRoot {
                root: root.clone(),
                base_commit: git_resolve_base_commit(&root.parent_root, base.as_ref()).await?,
                parent_root_dirty: !git_status_porcelain(&root.parent_root)
                    .await
                    .map_err(|error| {
                        AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
                    })?
                    .is_empty(),
            });
        }

        let mut created = Vec::<WorkbenchRoot>::new();
        for resolved in &resolved_roots {
            let root = &resolved.root;
            if let Err(error) = git_worktree_add(
                &root.parent_root,
                &root.worktree_root,
                &branch,
                &resolved.base_commit,
            )
            .await
            {
                let rollback_message = rollback_created_worktrees(&created, &branch).await;
                let message = append_rollback_message(error, rollback_message);
                return Err(AppError::internal_message(
                    OPERATION,
                    message.clone(),
                    anyhow!(message),
                ));
            }
            created.push(root.clone());
        }

        let project = {
            let mut project_store = project_store.lock().await;
            match project_store.create_workbench(
                payload.parent_project_id.clone(),
                payload.name,
                payload.branch,
                roots,
            ) {
                Ok(project) => project,
                Err(error) => {
                    let rollback_message = rollback_created_worktrees(&created, &branch).await;
                    let message = append_rollback_message(error.to_string(), rollback_message);
                    return Err(project_store_error(OPERATION, error.with_message(message)));
                }
            }
        };

        let project_id = project.id.clone();
        let mut state = self.state.lock().await;
        fan_out_project_notify(
            &mut state,
            ProjectNotifyPayload::Upsert {
                project: project.clone(),
            },
        )
        .await;
        // The workbench record and worktrees already exist and the Upsert
        // has fanned out, so a project-actor failure here must not fail
        // the request (mirrors create_project): warn and return success.
        if let Err(error) = ensure_project_actor(&mut state, project_id.clone()).await {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to start project actor after workbench creation"
            );
        }
        let host_paths = state.host_streams.keys().cloned().collect::<Vec<_>>();
        for host_path in host_paths {
            if let Err(error) =
                subscribe_host_to_project(&mut state, &host_path, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to subscribe host to workbench project stream after creation"
                );
            }
            if let Err(error) =
                emit_review_list_changed_for_project(&mut state, project_id.clone()).await
            {
                tracing::warn!(
                    host_stream = %host_path,
                    project_id = %project_id,
                    error = %error,
                    "failed to emit initial review list after workbench creation"
                );
            }
        }
        drop(state);
        self.update_workflow_watcher_targets_and_reload("workbench_create")
            .await?;
        Ok(CreatedWorkbench {
            project,
            roots: resolved_roots,
        })
    }

    pub(crate) async fn list_projects(&self) -> Result<Vec<Project>, String> {
        let state = self.state.lock().await;
        state.project_store.lock().await.list().map_err(Into::into)
    }

    pub(crate) async fn project_id_for_agent(&self, agent_id: &AgentId) -> Option<ProjectId> {
        self.agent_project_id(agent_id).await
    }

    pub(crate) async fn remove_workbench(&self, payload: WorkbenchRemovePayload) -> AppResult<()> {
        const OPERATION: &str = "workbench_remove";
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };

        let project = load_project(&project_store, &payload.id, OPERATION).await?;
        let ProjectSource::GitWorkbench {
            parent_project_id,
            roots,
            ..
        } = &project.source
        else {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "cannot remove non-workbench project {} as workbench",
                    payload.id
                ),
            ));
        };
        let parent_lock = self.workbench_parent_lock(parent_project_id).await;
        let _parent_guard = parent_lock.lock().await;

        // Mark the workbench as being removed before validating blockers
        // so agent spawns and terminal creation reject it while the slow
        // git subprocesses below run outside the host state lock. The
        // marker is cleared on every exit path (success or failure) by
        // the wrapper below.
        {
            let mut state = self.state.lock().await;
            if !state.removing_projects.insert(payload.id.clone()) {
                return Err(AppError::conflict(
                    OPERATION,
                    format!("workbench {} is already being removed", payload.id),
                ));
            }
        }
        wait_for_workbench_remove_test_hook(self).await;
        let result = self
            .remove_workbench_marked(&payload, &project, roots, &project_store)
            .await;
        self.state
            .lock()
            .await
            .removing_projects
            .remove(&payload.id);
        result
    }

    async fn remove_workbench_marked(
        &self,
        payload: &WorkbenchRemovePayload,
        project: &Project,
        roots: &[WorkbenchRoot],
        project_store: &Arc<Mutex<ProjectStore>>,
    ) -> AppResult<()> {
        const OPERATION: &str = "workbench_remove";
        self.validate_workbench_remove_blockers(project).await?;

        {
            let mut state = self.state.lock().await;
            if let Some(mut router) = state.code_intel_routers.remove(&payload.id) {
                router.shutdown_all();
            }
            if let Some(subscription) = state.project_streams.remove(&payload.id) {
                subscription.task.abort();
            }
        }

        for root in roots {
            let exists = tokio::fs::try_exists(&root.worktree_root.0)
                .await
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
            if exists {
                git_worktree_remove(&root.parent_root, &root.worktree_root)
                    .await
                    .map_err(|error| {
                        AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
                    })?;
            } else {
                // The worktree dir was deleted out of band (or by an
                // earlier half-failed removal); prune git's worktree
                // bookkeeping in the parent repo and keep going so the
                // record can still be removed. A retry after a partial
                // failure lands here and succeeds.
                tracing::warn!(
                    project_id = %project.id,
                    worktree_root = %root.worktree_root,
                    "worktree root missing during workbench removal; pruning git worktree bookkeeping"
                );
                if let Err(error) = git_worktree_prune(&root.parent_root).await {
                    tracing::warn!(
                        project_id = %project.id,
                        parent_root = %root.parent_root,
                        error = %error,
                        "failed to prune git worktrees for missing worktree root"
                    );
                }
            }
        }

        let deleted = {
            let mut project_store = project_store.lock().await;
            project_store
                .delete_workbench(&payload.id)
                .map_err(|error| project_store_error(OPERATION, error))?
        };

        let mut state = self.state.lock().await;
        cleanup_reviews_for_deleted_project(&state.review_registry, &payload.id).await;
        if let Some(mut router) = state.code_intel_routers.remove(&payload.id) {
            router.shutdown_all();
        }
        if let Some(subscription) = state.project_streams.remove(&payload.id) {
            subscription.task.abort();
        }
        fan_out_project_notify(
            &mut state,
            ProjectNotifyPayload::Delete { project: deleted },
        )
        .await;
        drop(state);
        self.update_workflow_watcher_targets_and_reload("workbench_remove")
            .await?;
        Ok(())
    }

    async fn validate_workbench_remove_blockers(&self, project: &Project) -> AppResult<()> {
        const OPERATION: &str = "workbench_remove";
        let ProjectSource::GitWorkbench {
            parent_project_id,
            roots,
            ..
        } = &project.source
        else {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "cannot remove non-workbench project {} as workbench",
                    project.id
                ),
            ));
        };

        let (
            agent_handles,
            terminal_handles,
            session_store,
            steering_store,
            project_store,
            team_registry,
        ) = {
            let state = self.state.lock().await;
            let agent_handles = state
                .registry
                .agent_ids()
                .into_iter()
                .filter_map(|agent_id| state.registry.agent_handle(&agent_id))
                .collect::<Vec<_>>();
            let terminal_handles = state.terminal_streams.values().cloned().collect::<Vec<_>>();
            (
                agent_handles,
                terminal_handles,
                Arc::clone(&state.session_store),
                Arc::clone(&state.steering_store),
                Arc::clone(&state.project_store),
                state.team_registry.clone(),
            )
        };

        for agent in agent_handles {
            let start = agent.snapshot();
            if start.project_id.as_ref() == Some(&project.id) {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot remove workbench {} while agent {} is live",
                        project.id, start.agent_id
                    ),
                ));
            }
        }

        for terminal in terminal_handles {
            if terminal.project_id() == Some(&project.id) && terminal.is_running() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot remove workbench {} while a terminal is live",
                        project.id
                    ),
                ));
            }
        }

        let referenced_session = session_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|session| session.project_id.as_ref() == Some(&project.id));
        if let Some(session) = referenced_session {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot remove workbench {} while referenced by session {}",
                    project.id, session.id
                ),
            ));
        }

        let referenced_steering = steering_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|steering| matches!(&steering.scope, SteeringScope::Project(project_id) if project_id == &project.id));
        if let Some(steering) = referenced_steering {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot remove workbench {} while referenced by steering {}",
                    project.id, steering.id
                ),
            ));
        }

        let snapshot = team_registry
            .snapshot()
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let referenced_team_members = snapshot
            .members
            .iter()
            .filter(|member| member.project_ids.contains(&project.id))
            .map(|member| member.id.clone())
            .collect::<Vec<_>>();
        if !referenced_team_members.is_empty() {
            return Err(AppError::conflict(
                OPERATION,
                referenced_team_member_delete_message(
                    "workbench",
                    &project.id,
                    Some(project.name.as_str()),
                    &snapshot,
                    &referenced_team_members,
                ),
            ));
        }

        let parent = {
            let project_store = project_store.lock().await;
            project_store.get(parent_project_id).ok_or_else(|| {
                AppError::internal_message(
                    OPERATION,
                    format!(
                        "workbench {} references missing parent project {}",
                        project.id, parent_project_id
                    ),
                    anyhow!("workbench parent record is missing"),
                )
            })?
        };
        let ProjectSource::Standalone {
            roots: parent_roots,
        } = &parent.source
        else {
            return Err(AppError::internal_message(
                OPERATION,
                format!(
                    "workbench {} parent project {} is not standalone",
                    project.id, parent_project_id
                ),
                anyhow!("workbench parent record is not standalone"),
            ));
        };
        let parent_root_set = parent_roots.iter().cloned().collect::<HashSet<_>>();

        for root in roots {
            if !parent_root_set.contains(&root.parent_root) {
                return Err(AppError::internal_message(
                    OPERATION,
                    format!(
                        "workbench {} parent root {} is missing from parent project {}",
                        project.id, root.parent_root, parent_project_id
                    ),
                    anyhow!("workbench parent root is missing"),
                ));
            }
        }

        let mut dirty_roots = Vec::new();
        for root in roots {
            let exists = tokio::fs::try_exists(&root.worktree_root.0)
                .await
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
            if !exists {
                // The worktree dir was deleted out of band (or by an
                // earlier half-failed removal). Treat it as removable
                // rather than blocking: removal prunes git bookkeeping
                // and deletes the record, so retries are the recourse.
                continue;
            }
            let status = git_status_porcelain(&root.worktree_root)
                .await
                .map_err(|error| {
                    AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
                })?;
            if !status.trim().is_empty() {
                dirty_roots.push(root.worktree_root.0.clone());
            }
        }
        if !dirty_roots.is_empty() {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot remove workbench {} because worktree roots are dirty: {}",
                    project.id,
                    dirty_roots.join(", ")
                ),
            ));
        }

        Ok(())
    }

    pub(crate) async fn upsert_custom_agent(
        &self,
        payload: CustomAgentUpsertPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "custom_agent_upsert";
        let mut state = self.state.lock().await;
        let custom_agent_id = payload.custom_agent.id.clone();
        let skills = state
            .skill_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let skill_ids = skills
            .into_iter()
            .map(|skill| skill.id)
            .collect::<HashSet<_>>();
        for skill_id in &payload.custom_agent.skill_ids {
            if !skill_ids.contains(skill_id) {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "custom agent {} references missing skill {}",
                        custom_agent_id, skill_id
                    ),
                ));
            }
        }

        let mcp_servers = state
            .mcp_server_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let mcp_server_ids = mcp_servers
            .into_iter()
            .map(|mcp_server| mcp_server.id)
            .collect::<HashSet<_>>();
        for mcp_server_id in &payload.custom_agent.mcp_server_ids {
            if !mcp_server_ids.contains(mcp_server_id) {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "custom agent {} references missing MCP server {}",
                        custom_agent_id, mcp_server_id
                    ),
                ));
            }
        }

        let custom_agent = state
            .custom_agent_store
            .lock()
            .await
            .upsert(payload.custom_agent)
            .map_err(|error| custom_agent_store_error(OPERATION, error))?;
        fan_out_custom_agent_notify(
            &mut state,
            CustomAgentNotifyPayload::Upsert { custom_agent },
        )
        .await;
        Ok(())
    }

    pub(crate) async fn delete_custom_agent(
        &self,
        payload: CustomAgentDeletePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "custom_agent_delete";
        let mut state = self.state.lock().await;
        let snapshot = state
            .team_registry
            .snapshot()
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        // Built-in team custom agents are wired into the role-preset
        // catalog as the default agent for each role. If we let those
        // records be deleted, the catalog would advertise stale ids that
        // drafts/templates would happily copy into draft members. Keep
        // invalid states unrepresentable by rejecting the delete.
        if snapshot
            .catalog
            .role_presets
            .iter()
            .any(|role| role.default_custom_agent_id.as_ref() == Some(&payload.id))
        {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot delete built-in custom agent {} that backs a team role preset",
                    payload.id
                ),
            ));
        }
        let referenced_team_members = snapshot
            .members
            .iter()
            .filter(|member| member.custom_agent_id.as_ref() == Some(&payload.id))
            .map(|member| member.id.clone())
            .collect::<Vec<_>>();
        if !referenced_team_members.is_empty() {
            let custom_agent_name = state
                .custom_agent_store
                .lock()
                .await
                .get(&payload.id)
                .map(|custom_agent| custom_agent.name);
            return Err(AppError::conflict(
                OPERATION,
                referenced_team_member_delete_message(
                    "custom agent",
                    &payload.id,
                    custom_agent_name.as_deref(),
                    &snapshot,
                    &referenced_team_members,
                ),
            ));
        }
        let id = state
            .custom_agent_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| custom_agent_store_error(OPERATION, error))?;
        fan_out_custom_agent_notify(&mut state, CustomAgentNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn upsert_steering(&self, payload: SteeringUpsertPayload) -> AppResult<()> {
        const OPERATION: &str = "steering_upsert";
        let mut state = self.state.lock().await;
        if let SteeringScope::Project(project_id) = &payload.steering.scope
            && !project_exists(&state.project_store, project_id, OPERATION).await?
        {
            return Err(AppError::not_found(
                OPERATION,
                format!(
                    "cannot upsert project-scoped steering {} for missing project {}",
                    payload.steering.id, project_id
                ),
            ));
        }
        let steering = state
            .steering_store
            .lock()
            .await
            .upsert(payload.steering)
            .map_err(|error| steering_store_error(OPERATION, error))?;
        fan_out_steering_notify(&mut state, SteeringNotifyPayload::Upsert { steering }).await;
        Ok(())
    }

    pub(crate) async fn delete_steering(&self, payload: SteeringDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "steering_delete";
        let mut state = self.state.lock().await;
        let id = state
            .steering_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| steering_store_error(OPERATION, error))?;
        fan_out_steering_notify(&mut state, SteeringNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn refresh_skills(&self, _payload: SkillRefreshPayload) -> AppResult<()> {
        const OPERATION: &str = "skill_refresh";
        let mut state = self.state.lock().await;
        let sync = state
            .skill_store
            .lock()
            .await
            .sync_from_disk()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        for id in sync.deletes {
            fan_out_skill_notify(&mut state, SkillNotifyPayload::Delete { id }).await;
        }
        for skill in sync.upserts {
            fan_out_skill_notify(&mut state, SkillNotifyPayload::Upsert { skill }).await;
        }
        Ok(())
    }

    pub(crate) async fn upsert_skill(&self, skill: Skill, body: String) -> AppResult<()> {
        const OPERATION: &str = "skill_upsert";
        let mut state = self.state.lock().await;
        let skill = state
            .skill_store
            .lock()
            .await
            .upsert(skill, body)
            .map_err(|error| skill_store_error(OPERATION, error))?;
        fan_out_skill_notify(&mut state, SkillNotifyPayload::Upsert { skill }).await;
        Ok(())
    }

    pub(crate) async fn delete_skill(&self, id: protocol::SkillId) -> AppResult<()> {
        const OPERATION: &str = "skill_delete";
        let mut state = self.state.lock().await;
        let id = state
            .skill_store
            .lock()
            .await
            .delete(&id)
            .map_err(|error| skill_store_error(OPERATION, error))?;
        fan_out_skill_notify(&mut state, SkillNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn upsert_mcp_server(&self, payload: McpServerUpsertPayload) -> AppResult<()> {
        const OPERATION: &str = "mcp_server_upsert";
        let mut state = self.state.lock().await;
        let name = payload.mcp_server.name.trim();
        if RESERVED_MCP_SERVER_NAMES.contains(&name) {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "MCP server {} name '{}' is reserved",
                    payload.mcp_server.id, name
                ),
            ));
        }
        let mcp_server = state
            .mcp_server_store
            .lock()
            .await
            .upsert(payload.mcp_server)
            .map_err(|error| mcp_server_store_error(OPERATION, error))?;
        fan_out_mcp_server_notify(&mut state, McpServerNotifyPayload::Upsert { mcp_server }).await;
        Ok(())
    }

    pub(crate) async fn delete_mcp_server(&self, payload: McpServerDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "mcp_server_delete";
        let mut state = self.state.lock().await;
        let referenced_agent = state
            .custom_agent_store
            .lock()
            .await
            .list()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .into_iter()
            .find(|custom_agent| custom_agent.mcp_server_ids.contains(&payload.id));
        if let Some(custom_agent) = referenced_agent {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "cannot delete MCP server {} while it is referenced by custom agent {}",
                    payload.id, custom_agent.id
                ),
            ));
        }
        let id = state
            .mcp_server_store
            .lock()
            .await
            .delete(&payload.id)
            .map_err(|error| mcp_server_store_error(OPERATION, error))?;
        fan_out_mcp_server_notify(&mut state, McpServerNotifyPayload::Delete { id }).await;
        Ok(())
    }

    pub(crate) async fn describe_team_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<TeamDescribeData, String> {
        let registry = { self.state.lock().await.team_registry.clone() };
        registry.describe_for_agent(agent_id).await
    }

    pub(crate) async fn list_custom_agents(&self) -> Result<Vec<CustomAgent>, String> {
        let store = { Arc::clone(&self.state.lock().await.custom_agent_store) };
        let agents = store.lock().await.list()?;
        Ok(agents)
    }

    pub(crate) async fn list_skills(&self) -> Result<Vec<protocol::Skill>, String> {
        let store = { Arc::clone(&self.state.lock().await.skill_store) };
        let skills = store.lock().await.list()?;
        Ok(skills)
    }

    pub(crate) async fn list_mcp_servers(&self) -> Result<Vec<protocol::McpServerConfig>, String> {
        let store = { Arc::clone(&self.state.lock().await.mcp_server_store) };
        let servers = store.lock().await.list()?;
        Ok(servers)
    }

    pub(crate) async fn custom_agent_by_id(
        &self,
        id: &protocol::CustomAgentId,
    ) -> Result<Option<CustomAgent>, String> {
        let store = { Arc::clone(&self.state.lock().await.custom_agent_store) };
        Ok(store.lock().await.get(id))
    }

    /// User-initiated team-member activation (host stream). Mirrors the
    /// Reuse/Resume/New branches of `message_team_member` but skips the
    /// caller-is-manager auth. With `prompt: None`, the no-binding + no-session
    /// case is a no-op (just opens the chat tab; first user message will
    /// re-send with `prompt: Some`).
    pub(crate) async fn activate_team_member(
        &self,
        member_id: TeamMemberId,
        prompt: Option<String>,
        images: Option<Vec<ImageData>>,
    ) -> AppResult<TeamMemberMessageOutcome> {
        const OPERATION: &str = "team_member_activate";
        let registry = { self.state.lock().await.team_registry.clone() };
        let has_prompt = prompt.is_some();
        let plan = registry
            .plan_user_activation(member_id.clone(), has_prompt)
            .await
            .map_err(|error| team_member_activation_error(OPERATION, error))?;
        match plan.activation.clone() {
            TeamMemberActivation::Reuse { agent_id } => {
                if let Some(prompt) = prompt {
                    self.message_bound_team_member(&registry, &plan, agent_id, prompt, images)
                        .await
                        .map_err(|error| team_member_activation_error(OPERATION, error))
                } else {
                    Ok(TeamMemberMessageOutcome {
                        member_id: plan.member.id.clone(),
                        agent_id,
                        queued: false,
                    })
                }
            }
            TeamMemberActivation::Resume { session_id } => {
                if !has_prompt {
                    // Defer until a real message arrives.
                    return Ok(TeamMemberMessageOutcome {
                        member_id: plan.member.id.clone(),
                        agent_id: AgentId(String::new()),
                        queued: false,
                    });
                }
                if let Err(err) = self.ensure_team_resume_session(&session_id).await {
                    self.record_team_member_resume_failure(&registry, plan.member.id.clone())
                        .await
                        .map_err(|error| team_member_activation_error(OPERATION, error))?;
                    return Err(team_member_activation_error(OPERATION, err));
                }
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::Resume { session_id, prompt },
                )
                .await
                .map_err(|error| team_member_activation_error(OPERATION, error))
            }
            TeamMemberActivation::New => {
                let Some(message) = prompt else {
                    // Fresh member + no prompt: nothing to do server-side.
                    return Ok(TeamMemberMessageOutcome {
                        member_id: plan.member.id.clone(),
                        agent_id: AgentId(String::new()),
                        queued: false,
                    });
                };
                let prompt = if plan.member.role == TeamMemberRole::Manager
                    && plan.member.session_id.is_none()
                {
                    match self.manager_prompt_with_roster(&plan, message).await {
                        Ok(prompt) => prompt,
                        Err(err) => {
                            let events = registry
                                .record_binding_failure(plan.member.id.clone())
                                .await
                                .map_err(|error| team_member_activation_error(OPERATION, error))?;
                            self.fan_out_team_registry_events(events).await;
                            return Err(team_member_activation_error(OPERATION, err));
                        }
                    }
                } else {
                    message
                };
                let backend_kind = plan.member.backend_kind;
                let workspace_roots = match self.team_member_workspace_roots(&plan.member).await {
                    Ok(workspace_roots) => workspace_roots,
                    Err(err) => {
                        let events = registry
                            .record_binding_failure(plan.member.id.clone())
                            .await
                            .map_err(|error| team_member_activation_error(OPERATION, error))?;
                        self.fan_out_team_registry_events(events).await;
                        return Err(team_member_activation_error(OPERATION, err));
                    }
                };
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::New {
                        workspace_roots,
                        prompt,
                        images,
                        backend_kind,
                        launch_profile_id: None,
                        cost_hint: plan.member.cost_hint,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                )
                .await
                .map_err(|error| team_member_activation_error(OPERATION, error))
            }
        }
    }

    pub(crate) async fn message_team_member(
        &self,
        caller_agent_id: AgentId,
        member_id: TeamMemberId,
        message: String,
        images: Option<Vec<ImageData>>,
    ) -> Result<TeamMemberMessageOutcome, String> {
        let registry = { self.state.lock().await.team_registry.clone() };
        let plan = registry
            .plan_message_member(caller_agent_id, member_id.clone())
            .await?;
        match plan.activation.clone() {
            TeamMemberActivation::Reuse { agent_id } => {
                self.message_bound_team_member(&registry, &plan, agent_id, message, images)
                    .await
            }
            TeamMemberActivation::Resume { session_id } => {
                if let Err(err) = self.ensure_team_resume_session(&session_id).await {
                    self.record_team_member_resume_failure(&registry, plan.member.id.clone())
                        .await?;
                    return Err(err);
                }
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::Resume {
                        session_id,
                        prompt: Some(message),
                    },
                )
                .await
            }
            TeamMemberActivation::New => {
                let prompt = if plan.member.role == TeamMemberRole::Manager
                    && plan.member.session_id.is_none()
                {
                    match self.manager_prompt_with_roster(&plan, message).await {
                        Ok(prompt) => prompt,
                        Err(err) => {
                            let events = registry
                                .record_binding_failure(plan.member.id.clone())
                                .await?;
                            self.fan_out_team_registry_events(events).await;
                            return Err(err);
                        }
                    }
                } else {
                    message
                };
                let backend_kind = plan.member.backend_kind;
                let workspace_roots = match self.team_member_workspace_roots(&plan.member).await {
                    Ok(workspace_roots) => workspace_roots,
                    Err(err) => {
                        let events = registry
                            .record_binding_failure(plan.member.id.clone())
                            .await?;
                        self.fan_out_team_registry_events(events).await;
                        return Err(err);
                    }
                };
                self.spawn_unbound_team_member(
                    &registry,
                    &plan,
                    SpawnAgentParams::New {
                        workspace_roots,
                        prompt,
                        images,
                        backend_kind,
                        launch_profile_id: None,
                        cost_hint: plan.member.cost_hint,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                )
                .await
            }
        }
    }

    async fn message_bound_team_member(
        &self,
        registry: &TeamRegistryHandle,
        plan: &TeamMessagePlan,
        agent_id: AgentId,
        message: String,
        images: Option<Vec<ImageData>>,
    ) -> Result<TeamMemberMessageOutcome, String> {
        let handle = self.agent_handle(&agent_id).await.ok_or_else(|| {
            format!(
                "team member {} is bound to missing agent {agent_id}",
                plan.member.id
            )
        })?;
        let queued = self
            .agent_status_snapshot(&agent_id)
            .await
            .map(|status| status.is_active() || status.is_plan_approval_pending())
            .unwrap_or(false);
        let events = registry
            .record_member_activity(plan.member.id.clone(), AgentControlStatus::Thinking)
            .await?;
        self.fan_out_team_registry_events(events).await;
        let sent = handle
            .send_input(AgentInput::SendMessage(SendMessagePayload {
                message,
                images,
                origin: None,
                tool_response: None,
            }))
            .await;
        if !sent {
            let events = registry
                .record_binding_failure(plan.member.id.clone())
                .await?;
            self.fan_out_team_registry_events(events).await;
            return Err(format!(
                "team member {} agent backend is closed",
                plan.member.id
            ));
        }
        Ok(TeamMemberMessageOutcome {
            member_id: plan.member.id.clone(),
            agent_id,
            queued,
        })
    }

    async fn spawn_unbound_team_member(
        &self,
        registry: &TeamRegistryHandle,
        plan: &TeamMessagePlan,
        params: SpawnAgentParams,
    ) -> Result<TeamMemberMessageOutcome, String> {
        let clear_session_on_failure = matches!(&params, SpawnAgentParams::Resume { .. });
        let payload = SpawnAgentPayload {
            name: Some(plan.member.name.clone()),
            custom_agent_id: plan.member.custom_agent_id.clone(),
            parent_agent_id: None,
            project_id: Some(team_member_primary_project_id(&plan.member)?),
            params,
        };
        let agent_id = self
            .spawn_agent_with_origin_config_and_team(
                payload,
                AgentOrigin::TeamMember,
                None,
                Some(TeamSpawnContext {
                    team_id: plan.team.id.clone(),
                    team_member_id: plan.member.id.clone(),
                }),
                None,
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        match self.wait_for_agent_session_id_result(&agent_id).await {
            Ok(session_id) => {
                let refs = {
                    let state = self.state.lock().await;
                    agent_team_validation_refs(&state, "team_member_bind")
                        .await
                        .map_err(|err| err.to_string())?
                };
                let events = registry
                    .bind_member_agent(
                        plan.member.id.clone(),
                        agent_id.clone(),
                        Some(session_id),
                        refs,
                    )
                    .await?;
                self.fan_out_team_registry_events(events).await;
                if let Some(status) = self.agent_status_snapshot(&agent_id).await {
                    let events = if status.terminated {
                        registry.clear_binding_by_agent(agent_id.clone()).await?
                    } else if status.is_plan_approval_pending() {
                        registry
                            .record_agent_activity(agent_id.clone(), AgentControlStatus::Thinking)
                            .await?
                    } else {
                        registry
                            .record_agent_activity(agent_id.clone(), status.status())
                            .await?
                    };
                    self.fan_out_team_registry_events(events).await;
                }
                Ok(TeamMemberMessageOutcome {
                    member_id: plan.member.id.clone(),
                    agent_id,
                    queued: false,
                })
            }
            Err(err) => {
                if clear_session_on_failure {
                    self.record_team_member_resume_failure(registry, plan.member.id.clone())
                        .await?;
                } else {
                    let events = registry
                        .record_binding_failure(plan.member.id.clone())
                        .await?;
                    self.fan_out_team_registry_events(events).await;
                }
                Err(err)
            }
        }
    }

    async fn record_team_member_resume_failure(
        &self,
        registry: &TeamRegistryHandle,
        member_id: TeamMemberId,
    ) -> Result<(), String> {
        let refs = {
            let state = self.state.lock().await;
            agent_team_validation_refs(&state, "team_member_resume_failure")
                .await
                .map_err(|err| err.to_string())?
        };
        let events = registry.record_resume_failure(member_id, refs).await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    async fn wait_for_agent_session_id_result(
        &self,
        agent_id: &AgentId,
    ) -> Result<SessionId, String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let (session_id, status_handle) = {
                let state = self.state.lock().await;
                (
                    state.agent_sessions.get(agent_id).cloned(),
                    state.registry.agent_status_handle(agent_id),
                )
            };
            if let Some(session_id) = session_id {
                return Ok(session_id);
            }
            if let Some(status_handle) = status_handle {
                let status = status_handle.snapshot().await;
                if status.terminated {
                    return Err(status.last_error.unwrap_or_else(|| {
                        format!("agent {agent_id} terminated before session binding")
                    }));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for team member agent {agent_id} session binding"
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn ensure_team_resume_session(&self, session_id: &SessionId) -> Result<(), String> {
        let (session_store, antigravity_conversations_dir) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_store),
                state.antigravity_conversations_dir.clone(),
            )
        };
        let record = session_store
            .lock()
            .await
            .list()
            .map_err(|error| format!("failed to load sessions before team resume: {error}"))?
            .into_iter()
            .find(|record| record.id == *session_id)
            .ok_or_else(|| format!("cannot resume missing session {session_id}"))?;
        if !session_record_is_resumable(&record, &antigravity_conversations_dir) {
            return Err(format!("cannot resume non-resumable session {session_id}"));
        }
        Ok(())
    }

    async fn team_member_workspace_roots(
        &self,
        member: &TeamMember,
    ) -> Result<Vec<String>, String> {
        if member.project_ids.is_empty() {
            return Err(format!("team member {} has no project_ids", member.id));
        }
        let project_store = { Arc::clone(&self.state.lock().await.project_store) };
        let project_store = project_store.lock().await;
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for project_id in &member.project_ids {
            let project = project_store.get(project_id).ok_or_else(|| {
                format!(
                    "team member {} references missing project {}",
                    member.id, project_id
                )
            })?;
            for root in project.root_paths() {
                let root = root.0;
                if seen.insert(root.clone()) {
                    roots.push(root);
                }
            }
        }
        Ok(roots)
    }

    async fn manager_prompt_with_roster(
        &self,
        plan: &TeamMessagePlan,
        prompt: String,
    ) -> Result<String, String> {
        let registry = { self.state.lock().await.team_registry.clone() };
        let snapshot = registry.snapshot().await?;
        let members = snapshot
            .members
            .into_iter()
            .filter(|member| member.team_id == plan.team.id)
            .collect::<Vec<_>>();
        Ok(prepend_manager_roster(&plan.team, &members, prompt))
    }

    pub(crate) async fn create_team(&self, payload: TeamCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "team_create";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.create_team(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn rename_team(&self, payload: TeamRenamePayload) -> AppResult<()> {
        const OPERATION: &str = "team_rename";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.rename_team(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn delete_team(&self, payload: TeamDeletePayload) -> AppResult<()> {
        const OPERATION: &str = "team_delete";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.delete_team(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn set_team_manager(&self, payload: TeamSetManagerPayload) -> AppResult<()> {
        const OPERATION: &str = "team_set_manager";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.set_manager(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn create_team_member(
        &self,
        payload: TeamMemberCreatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_create";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.create_member(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn update_team_member(
        &self,
        payload: TeamMemberUpdatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_update";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.update_member(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn delete_team_member(
        &self,
        payload: TeamMemberDeletePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_delete";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.delete_member(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn create_team_draft(&self, payload: TeamDraftCreatePayload) -> AppResult<()> {
        const OPERATION: &str = "team_draft_create";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .create_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn update_team_draft(&self, payload: TeamDraftUpdatePayload) -> AppResult<()> {
        const OPERATION: &str = "team_draft_update";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .update_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn shuffle_team_draft(
        &self,
        payload: TeamDraftShufflePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_draft_shuffle";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .shuffle_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn shuffle_team_member(
        &self,
        payload: TeamMemberShufflePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_member_shuffle";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .shuffle_member_suggestion(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn apply_team_draft_template(
        &self,
        payload: TeamDraftApplyTemplatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_draft_apply_template";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .apply_draft_template(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn commit_team_draft(&self, payload: TeamDraftCommitPayload) -> AppResult<()> {
        const OPERATION: &str = "team_draft_commit";
        let events = self
            .serialized_team_registry_mutation(OPERATION, |registry, refs| async move {
                registry.commit_draft(payload, refs).await
            })
            .await?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    pub(crate) async fn discard_team_draft(
        &self,
        payload: TeamDraftDiscardPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "team_draft_discard";
        let registry = { self.state.lock().await.team_registry.clone() };
        let events = registry
            .discard_draft(payload)
            .await
            .map_err(|error| team_registry_error(OPERATION, error))?;
        self.fan_out_team_registry_events(events).await;
        Ok(())
    }

    async fn serialized_team_registry_mutation<F, Fut>(
        &self,
        operation: &'static str,
        mutate: F,
    ) -> AppResult<TeamRegistryEvents>
    where
        F: FnOnce(TeamRegistryHandle, AgentTeamValidationRefs) -> Fut,
        Fut: Future<Output = Result<TeamRegistryEvents, String>>,
    {
        let state = self.state.lock().await;
        let registry = state.team_registry.clone();
        let refs = agent_team_validation_refs(&state, operation).await?;
        #[cfg(test)]
        wait_for_team_mutation_after_refs_test_hook(self, operation).await;
        // Hold host_state through the registry mutation so the validation-ref
        // snapshot and persisted team change serialize with project/custom-agent deletes.
        let events = mutate(registry, refs)
            .await
            .map_err(|error| team_registry_error(operation, error))?;
        drop(state);
        Ok(events)
    }

    async fn fan_out_team_registry_events(&self, events: TeamRegistryEvents) {
        let mut state = self.state.lock().await;
        fan_out_team_registry_events(&mut state, events).await;
    }

    pub(crate) async fn list_sessions(
        &self,
        host_output_stream: &Stream,
        payload: ListSessionsPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "list_sessions";
        let (sessions, page) = if let Some(cursor) = payload.cursor {
            let state = self.state.lock().await;
            let subscriber = state
                .host_streams
                .get(host_output_stream.path())
                .ok_or_else(|| {
                    AppError::invalid(
                        OPERATION,
                        format!("unknown host stream {}", host_output_stream.path()),
                    )
                })?;
            page_existing_session_list_snapshot(
                subscriber,
                cursor,
                payload.scope,
                payload.limit,
                OPERATION,
            )?
        } else {
            let (session_store, scope, antigravity_conversations_dir) = {
                let state = self.state.lock().await;
                let subscriber = state
                    .host_streams
                    .get(host_output_stream.path())
                    .ok_or_else(|| {
                        AppError::invalid(
                            OPERATION,
                            format!("unknown host stream {}", host_output_stream.path()),
                        )
                    })?;
                (
                    Arc::clone(&state.session_store),
                    payload
                        .scope
                        .unwrap_or_else(|| subscriber.session_list_replay.default_scope()),
                    state.antigravity_conversations_dir.clone(),
                )
            };
            let sessions = session_store
                .lock()
                .await
                .summaries_for_scope_with_antigravity_conversations_dir(
                    scope,
                    &antigravity_conversations_dir,
                )
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
            let mut state = self.state.lock().await;
            let subscriber = state
                .host_streams
                .get_mut(host_output_stream.path())
                .ok_or_else(|| {
                    AppError::invalid(
                        OPERATION,
                        format!("unknown host stream {}", host_output_stream.path()),
                    )
                })?;
            replace_session_list_snapshot(subscriber, scope, sessions, payload.limit, OPERATION)?
        };

        let payload = SessionListPayload { sessions, page };
        let payload = serde_json::to_value(&payload).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize SessionList payload for host stream",
                error,
            )
        })?;
        let _ = host_output_stream.send_value(FrameKind::SessionList, payload);
        Ok(())
    }

    pub(crate) async fn delete_session(&self, session_id: SessionId) -> AppResult<()> {
        const OPERATION: &str = "delete_session";
        let (session_store, annotations_store) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.session_store),
                state.agents_view_preferences_store.clone(),
            )
        };
        session_store
            .lock()
            .await
            .delete(&session_id)
            .map_err(|error| session_store_error(OPERATION, error))?;
        if let Some(store) = annotations_store {
            store
                .lock()
                .await
                .remove_session(HostFilterId(LOCAL_HOST_ID.to_owned()), session_id.clone())
                .map_err(|error| AppError::invalid(OPERATION, error))?;
        }
        self.fan_out_session_lists().await;
        let mut state = self.state.lock().await;
        fan_out_current_agents_view_preferences(&mut state).await;
        Ok(())
    }

    pub(crate) async fn fan_out_session_lists(&self) {
        let mut state = self.state.lock().await;
        fan_out_session_lists(&mut state).await;
    }

    pub(crate) async fn fan_out_task_token_usages(&self) {
        let (handles, closed_snapshots, live_agent_ids, agent_sessions) = {
            let state = self.state.lock().await;
            task_token_usage_sources_for_state(&state)
        };
        let payloads = task_token_usage_rollups_from_handles(
            handles,
            closed_snapshots,
            &live_agent_ids,
            &agent_sessions,
        )
        .await;
        let mut state = self.state.lock().await;
        fan_out_task_token_usages(&mut state, payloads).await;
    }

    pub(crate) async fn set_setting(&self, payload: SetSettingPayload) -> AppResult<()> {
        const OPERATION: &str = "set_setting";
        if let protocol::HostSettingValue::BackendNativeSettings { backend, settings } =
            &payload.setting
        {
            match backend {
                BackendKind::Tycode => {
                    crate::backend::tycode::persist_native_settings(settings.clone())
                        .await
                        .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
                    // A Tycode save can create or delete settings profiles,
                    // which changes the session schema's `profile` Select —
                    // re-publish it alongside the settings snapshot.
                    self.refresh_backend_config_snapshots_after_native_save()
                        .await;
                    self.refresh_session_schemas_with_fanout(true).await;
                }
                BackendKind::Hermes => {
                    let workspace_root = hermes_probe_workspace_root()
                        .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
                    crate::backend::hermes::persist_native_settings(
                        settings.clone(),
                        &[workspace_root],
                    )
                    .await
                    .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
                    // A Hermes save can change credentials and model defaults,
                    // so the session schema (model options per profile) must
                    // re-probe alongside the settings snapshot.
                    self.refresh_backend_config_snapshots_after_native_save()
                        .await;
                    self.refresh_session_schemas_with_fanout(true).await;
                }
                _ => {
                    return Err(AppError::invalid(
                        OPERATION,
                        format!("{backend:?} does not support backend-native settings saves"),
                    ));
                }
            }
            return Ok(());
        }

        if let protocol::HostSettingValue::BackendConfig { backend, values } = &payload.setting
            && *backend == BackendKind::Tycode
        {
            let settings_store = {
                let state = self.state.lock().await;
                Arc::clone(&state.settings_store)
            };
            let previous = settings_store
                .lock()
                .await
                .get()
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
                .backend_config
                .get(backend)
                .map(|values| crate::backend::sanitize_backend_config_values(*backend, values))
                .unwrap_or_default();
            let incoming = crate::backend::validate_backend_config_values(*backend, values)
                .map_err(|error| AppError::invalid(OPERATION, error))?;
            let persistence_values =
                crate::backend::tycode::tycode_backend_config_persistence_values(
                    &incoming, &previous,
                );
            crate::backend::tycode::persist_backend_config(persistence_values)
                .await
                .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        }

        if let protocol::HostSettingValue::MobileBrokerUrl { broker_url } = &payload.setting {
            crate::store::settings::validate_mobile_broker_url_for_write(broker_url.as_ref())
                .map_err(|error| AppError::invalid(OPERATION, error))?;
        }

        if let protocol::HostSettingValue::BackendTiers { backend, config } = &payload.setting
            && (!config.low.0.is_empty() || !config.high.0.is_empty())
        {
            let schema = {
                let state = self.state.lock().await;
                session_schema_for_backend(&state, *backend)
            }
            .ok_or_else(|| {
                AppError::invalid(
                    OPERATION,
                    format!("{backend:?} session settings schema unavailable"),
                )
            })?;
            validate_session_settings_values(&schema, &config.low).map_err(|error| {
                AppError::invalid(OPERATION, format!("invalid Low tier: {error}"))
            })?;
            validate_session_settings_values(&schema, &config.high).map_err(|error| {
                AppError::invalid(OPERATION, format!("invalid High tier: {error}"))
            })?;
        }

        let mut state = self.state.lock().await;
        let refresh_session_schemas = matches!(
            &payload.setting,
            protocol::HostSettingValue::EnabledBackends { .. }
        );
        let refresh_backend_config_snapshots = matches!(
            &payload.setting,
            protocol::HostSettingValue::EnabledBackends { .. }
                | protocol::HostSettingValue::BackendConfig {
                    backend: BackendKind::Tycode,
                    ..
                }
        );
        let settings = state
            .settings_store
            .lock()
            .await
            .apply(payload.setting)
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        fan_out_host_settings(&mut state, settings.clone()).await;
        apply_agent_activity_summary_setting(&mut state, &settings).await;
        apply_agent_supervisor_setting(&mut state, &settings);
        state.mobile_access.settings_changed(settings);
        fan_out_launch_profile_catalog(&mut state).await;
        if refresh_session_schemas || refresh_backend_config_snapshots {
            drop(state);
            if refresh_session_schemas {
                self.refresh_session_schemas().await;
            }
            if refresh_backend_config_snapshots {
                self.refresh_backend_config_snapshots().await;
            }
        }
        Ok(())
    }

    pub(crate) async fn set_agents_view_preferences(
        &self,
        payload: SetAgentsViewPreferencesPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "set_agents_view_preferences";
        let mut state = self.state.lock().await;
        let Some(store) = state.agents_view_preferences_store.clone() else {
            return Err(AppError::invalid(
                OPERATION,
                "agents view preferences are owned by the primary local host",
            ));
        };
        let update = Self::canonicalize_agents_view_preferences_update(&state, payload.update)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        let mut snapshot = store
            .lock()
            .await
            .apply(update)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        complete_agents_view_preferences_snapshot(&state, &mut snapshot).await;
        fan_out_agents_view_preferences(&mut state, snapshot).await;
        Ok(())
    }

    pub(crate) async fn set_agents_smart_views(
        &self,
        payload: SetAgentsSmartViewsPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "set_agents_smart_views";
        let mut state = self.state.lock().await;
        let Some(store) = state.agents_view_preferences_store.clone() else {
            return Err(AppError::invalid(
                OPERATION,
                "agents view preferences are owned by the primary local host",
            ));
        };
        let mut snapshot = store
            .lock()
            .await
            .apply_smart_views(payload.update)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        complete_agents_view_preferences_snapshot(&state, &mut snapshot).await;
        fan_out_agents_view_preferences(&mut state, snapshot).await;
        Ok(())
    }

    pub(crate) async fn set_agent_tags(&self, payload: SetAgentTagsPayload) -> AppResult<()> {
        const OPERATION: &str = "set_agent_tags";
        let mut state = self.state.lock().await;
        let Some(store) = state.agents_view_preferences_store.clone() else {
            return Err(AppError::invalid(
                OPERATION,
                "agents view preferences are owned by the primary local host",
            ));
        };
        let resolver = AnnotationTargetResolver::new(&state);
        let update = canonicalize_agent_tags_update(payload.update, &resolver)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        let mut snapshot = store
            .lock()
            .await
            .apply_tags(update, |target| resolver.canonicalize(target))
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        complete_agents_view_preferences_snapshot(&state, &mut snapshot).await;
        fan_out_agents_view_preferences(&mut state, snapshot).await;
        Ok(())
    }

    pub(crate) async fn set_agent_pins(&self, payload: SetAgentPinsPayload) -> AppResult<()> {
        const OPERATION: &str = "set_agent_pins";
        let mut state = self.state.lock().await;
        let Some(store) = state.agents_view_preferences_store.clone() else {
            return Err(AppError::invalid(
                OPERATION,
                "agents view preferences are owned by the primary local host",
            ));
        };
        let resolver = AnnotationTargetResolver::new(&state);
        let update = canonicalize_agent_pins_update(payload.update, &resolver)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        let mut snapshot = store
            .lock()
            .await
            .apply_pins(update, |target| resolver.canonicalize(target))
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        complete_agents_view_preferences_snapshot(&state, &mut snapshot).await;
        fan_out_agents_view_preferences(&mut state, snapshot).await;
        Ok(())
    }

    pub(crate) async fn set_agent_groups(&self, payload: SetAgentGroupsPayload) -> AppResult<()> {
        const OPERATION: &str = "set_agent_groups";
        let mut state = self.state.lock().await;
        let Some(store) = state.agents_view_preferences_store.clone() else {
            return Err(AppError::invalid(
                OPERATION,
                "agents view preferences are owned by the primary local host",
            ));
        };
        let resolver = AnnotationTargetResolver::new(&state);
        let update = canonicalize_agent_groups_update(payload.update, &resolver)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        let mut snapshot = store
            .lock()
            .await
            .apply_groups(update, |target| resolver.canonicalize(target))
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        complete_agents_view_preferences_snapshot(&state, &mut snapshot).await;
        fan_out_agents_view_preferences(&mut state, snapshot).await;
        Ok(())
    }

    fn canonicalize_agents_view_preferences_update(
        state: &HostState,
        update: AgentsViewPreferencesUpdate,
    ) -> Result<AgentsViewPreferencesUpdate, String> {
        match update {
            AgentsViewPreferencesUpdate::SetManualOrder { manual_order } => {
                Ok(AgentsViewPreferencesUpdate::SetManualOrder {
                    manual_order: Self::canonicalize_agent_manual_order(state, manual_order)?,
                })
            }
            other => Ok(other),
        }
    }

    fn canonicalize_agent_manual_order(
        state: &HostState,
        manual_order: Vec<AgentOrderKey>,
    ) -> Result<Vec<AgentOrderKey>, String> {
        let mut live_sessions = HashMap::<AgentId, Option<SessionId>>::new();
        for agent_id in state.registry.agent_ids() {
            let session_id = state.agent_sessions.get(&agent_id).cloned();
            live_sessions.insert(agent_id, session_id);
        }

        let mut seen = HashSet::new();
        let mut canonical = Vec::new();
        for key in manual_order {
            let key = match key {
                AgentOrderKey::Session { session_id } => {
                    Self::ensure_non_empty_agent_order_field(
                        "manual_order.session_id",
                        session_id.0.as_str(),
                    )?;
                    AgentOrderKey::Session { session_id }
                }
                AgentOrderKey::TransientAgent { host_id, agent_id } => {
                    Self::ensure_non_empty_agent_order_field(
                        "manual_order.host_id",
                        host_id.0.as_str(),
                    )?;
                    Self::ensure_non_empty_agent_order_field(
                        "manual_order.agent_id",
                        agent_id.0.as_str(),
                    )?;
                    if host_id.0 != LOCAL_HOST_ID {
                        // The primary-local preferences store cannot verify or
                        // rewrite remote live-agent ids; only session-keyed
                        // order is durable across host boundaries.
                        continue;
                    }
                    let Some(session_id) = live_sessions.get(&agent_id) else {
                        continue;
                    };
                    match session_id {
                        Some(session_id) => AgentOrderKey::Session {
                            session_id: session_id.clone(),
                        },
                        None => AgentOrderKey::TransientAgent { host_id, agent_id },
                    }
                }
            };
            if seen.insert(key.clone()) {
                canonical.push(key);
            }
        }
        Ok(canonical)
    }

    fn ensure_non_empty_agent_order_field(field: &str, value: &str) -> Result<(), String> {
        if value.trim().is_empty() {
            return Err(format!("{field} must not be empty"));
        }
        Ok(())
    }

    pub(crate) async fn start_mobile_pairing(&self, requester: StreamPath) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.start_pairing(requester)
    }

    pub(crate) async fn cancel_mobile_pairing(
        &self,
        payload: MobilePairingCancelPayload,
    ) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.cancel_pairing(payload)
    }

    pub(crate) async fn revoke_mobile_device(
        &self,
        payload: MobileDeviceRevokePayload,
    ) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.revoke_device(payload).await
    }

    pub(crate) async fn rename_mobile_device(
        &self,
        payload: MobileDeviceRenamePayload,
    ) -> AppResult<()> {
        let mobile_access = self.state.lock().await.mobile_access.clone();
        mobile_access.rename_device(payload).await
    }

    pub(crate) async fn fan_out_backend_setup(&self) {
        let payload = self.collect_backend_setup_respecting_probe().await;
        let mut state = self.state.lock().await;
        fan_out_backend_setup(&mut state, payload).await;
    }

    /// Collect backend setup, honoring the `skip_real_backend_probe` runtime
    /// flag. When skipping (test fixtures), returns an empty stub instantly
    /// instead of spawning real `<cli> --version` subprocesses and a codex
    /// model-discovery network RPC on every host spawn.
    async fn collect_backend_setup_respecting_probe(&self) -> BackendSetupPayload {
        let skip = self.state.lock().await.skip_real_backend_probe;
        if skip {
            setup::stub_backend_setup()
        } else {
            setup::collect_backend_setup().await
        }
    }

    fn schedule_session_schema_refresh(&self) {
        let host = self.clone();
        tokio::spawn(async move {
            host.refresh_pending_session_schemas().await;
        });
    }

    fn schedule_backend_config_snapshot_refresh(&self) {
        let host = self.clone();
        tokio::spawn(async move {
            host.refresh_backend_config_snapshots().await;
        });
    }

    pub(crate) async fn refresh_backend_config_snapshots(&self) {
        self.refresh_backend_config_snapshots_with_fanout(false)
            .await;
    }

    async fn refresh_backend_config_snapshots_after_native_save(&self) {
        self.refresh_backend_config_snapshots_with_fanout(true)
            .await;
    }

    async fn refresh_backend_config_snapshots_with_fanout(&self, force_emit: bool) {
        let (settings_store, skip_real_backend_probe) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.settings_store),
                state.skip_real_backend_probe,
            )
        };
        let enabled_backends = settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| {
                panic!("failed to load host settings for backend config snapshots: {err}")
            })
            .enabled_backends;
        let snapshots = if skip_real_backend_probe {
            BackendSettingsSnapshots::default()
        } else {
            backend_config_snapshots_for_enabled_backends(&enabled_backends).await
        };
        let mut state = self.state.lock().await;
        state.backend_config_snapshots = snapshots.backend_config;
        state.backend_native_settings_snapshots = snapshots.native_settings;
        fan_out_backend_config_snapshots(&mut state, force_emit).await;
    }

    pub(crate) async fn refresh_session_schemas(&self) {
        self.refresh_session_schemas_with_fanout(false).await;
    }

    pub(crate) async fn refresh_session_schema_for_backend(
        &self,
        backend_kind: protocol::BackendKind,
    ) {
        let _refresh_guard = self.session_schema_refresh_lock.lock().await;
        self.refresh_session_schemas_with_fanout_unlocked(false, true, Some(backend_kind))
            .await;
    }

    async fn resolve_session_schema_for_spawn(
        &self,
        backend_kind: protocol::BackendKind,
    ) -> Result<Option<SessionSettingsSchema>, AgentStartupFailure> {
        if !backend_has_dynamic_session_schema(backend_kind) {
            return Ok(Some(session_settings_schema_for_backend(backend_kind)));
        }
        let _refresh_guard = self.session_schema_refresh_lock.lock().await;
        let mut resolution = {
            let state = self.state.lock().await;
            session_schema_resolution_for_backend(&state, backend_kind)
        };
        if matches!(&resolution, SessionSchemaResolution::Pending) {
            self.refresh_session_schemas_with_fanout_unlocked(false, false, Some(backend_kind))
                .await;
            resolution = {
                let state = self.state.lock().await;
                session_schema_resolution_for_backend(&state, backend_kind)
            };
        }
        match resolution {
            SessionSchemaResolution::Ready(schema) => Ok(Some(schema)),
            SessionSchemaResolution::Unavailable(message) => {
                Err(AgentStartupFailure::backend_failed(format!(
                    "{backend_kind:?} session settings schema unavailable: {message}"
                )))
            }
            SessionSchemaResolution::Pending => Err(AgentStartupFailure::backend_failed(format!(
                "{backend_kind:?} session settings schema unavailable"
            ))),
        }
    }

    async fn refresh_session_schemas_with_fanout(&self, force_emit: bool) {
        let _refresh_guard = self.session_schema_refresh_lock.lock().await;
        self.refresh_session_schemas_with_fanout_unlocked(force_emit, true, None)
            .await;
    }

    async fn refresh_pending_session_schemas(&self) {
        let _refresh_guard = self.session_schema_refresh_lock.lock().await;
        let (settings_store, codex_pending, kiro_pending, hermes_pending) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.settings_store),
                matches!(
                    &state.codex_session_schema,
                    CodexSessionSchemaState::Pending
                ),
                matches!(&state.kiro_session_schema, KiroSessionSchemaState::Pending),
                matches!(
                    &state.hermes_session_schema,
                    HermesSessionSchemaState::Pending
                ),
            )
        };
        let enabled = settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for session schemas: {err}"))
            .enabled_backends;
        let pending = (codex_pending && enabled.contains(&protocol::BackendKind::Codex))
            || (kiro_pending && enabled.contains(&protocol::BackendKind::Kiro))
            || (hermes_pending && enabled.contains(&protocol::BackendKind::Hermes));
        if pending {
            self.refresh_session_schemas_with_fanout_unlocked(false, false, None)
                .await;
        }
    }

    async fn refresh_session_schemas_with_fanout_unlocked(
        &self,
        force_emit: bool,
        retry_unavailable: bool,
        only: Option<protocol::BackendKind>,
    ) {
        let probe = |kind: protocol::BackendKind| only.is_none_or(|scoped| scoped == kind);
        let (
            settings_store,
            codex_probe_program,
            kiro_probe_program,
            configured_kiro_probe_workspace_root,
            skip_real_backend_probe,
            previous_codex,
            previous_kiro,
            previous_hermes,
            prev_hermes_ready,
        ) = {
            let mut state = self.state.lock().await;
            #[cfg(feature = "test-support")]
            {
                state.session_schema_probe_count =
                    state.session_schema_probe_count.saturating_add(1);
            }
            let prev_hermes_ready = match &state.hermes_session_schema {
                HermesSessionSchemaState::Ready(schema) => Some(schema.clone()),
                HermesSessionSchemaState::Pending | HermesSessionSchemaState::Unavailable(_) => {
                    None
                }
            };
            let previous_codex = state.codex_session_schema.clone();
            let previous_kiro = state.kiro_session_schema.clone();
            let previous_hermes = state.hermes_session_schema.clone();
            if retry_unavailable {
                if probe(protocol::BackendKind::Codex) {
                    state.codex_session_schema = CodexSessionSchemaState::Pending;
                }
                if probe(protocol::BackendKind::Kiro) {
                    state.kiro_session_schema = KiroSessionSchemaState::Pending;
                }
                if probe(protocol::BackendKind::Hermes) {
                    state.hermes_session_schema = HermesSessionSchemaState::Pending;
                }
            }
            (
                Arc::clone(&state.settings_store),
                state.codex_probe_program.clone(),
                state.kiro_probe_program.clone(),
                state.kiro_probe_workspace_root.clone(),
                state.skip_real_backend_probe,
                previous_codex,
                previous_kiro,
                previous_hermes,
                prev_hermes_ready,
            )
        };
        let enabled_backends = settings_store
            .lock()
            .await
            .get()
            .unwrap_or_else(|err| panic!("failed to load host settings for session schemas: {err}"))
            .enabled_backends;

        let codex_session_schema = if !probe(protocol::BackendKind::Codex)
            || (!retry_unavailable && !matches!(&previous_codex, CodexSessionSchemaState::Pending))
        {
            previous_codex
        } else if enabled_backends.contains(&protocol::BackendKind::Codex) {
            if skip_real_backend_probe && codex_probe_program.is_none() {
                CodexSessionSchemaState::Unavailable(
                    "Codex model discovery is unavailable because backend probing is disabled"
                        .to_string(),
                )
            } else {
                match crate::backend::codex::probe_session_settings_schema(
                    codex_probe_program.as_deref(),
                )
                .await
                {
                    Ok(schema) => CodexSessionSchemaState::Ready(schema),
                    Err(err) => {
                        tracing::error!("failed to refresh Codex session schema: {err}");
                        CodexSessionSchemaState::Unavailable(err)
                    }
                }
            }
        } else {
            CodexSessionSchemaState::Pending
        };

        let kiro_session_schema = if !probe(protocol::BackendKind::Kiro)
            || (!retry_unavailable && !matches!(&previous_kiro, KiroSessionSchemaState::Pending))
        {
            previous_kiro
        } else if enabled_backends.contains(&protocol::BackendKind::Kiro) {
            match kiro_probe_workspace_root(configured_kiro_probe_workspace_root.as_deref()) {
                Ok(workspace_root) => match crate::backend::kiro::probe_session_settings_schema(
                    &[workspace_root],
                    kiro_probe_program,
                )
                .await
                {
                    Ok(schema) => KiroSessionSchemaState::Ready(schema),
                    Err(err) => {
                        tracing::error!("failed to refresh Kiro session schema: {err}");
                        KiroSessionSchemaState::Unavailable(err)
                    }
                },
                Err(err) => {
                    tracing::error!("failed to resolve Kiro probe workspace root: {err}");
                    KiroSessionSchemaState::Unavailable(err)
                }
            }
        } else {
            KiroSessionSchemaState::Pending
        };

        // `None` keeps the previously discovered Hermes profiles (when the
        // schema itself is kept); `Some` replaces them alongside the schema.
        let (hermes_session_schema, hermes_profiles) = if !probe(protocol::BackendKind::Hermes)
            || (!retry_unavailable
                && !matches!(&previous_hermes, HermesSessionSchemaState::Pending))
        {
            (previous_hermes, None)
        } else if enabled_backends.contains(&protocol::BackendKind::Hermes) {
            if skip_real_backend_probe {
                (
                    HermesSessionSchemaState::Ready(
                        <crate::backend::hermes::HermesBackend as crate::backend::Backend>::session_settings_schema(),
                    ),
                    Some(Vec::new()),
                )
            } else {
                let hermes_schema_or_last_good = |err: String| match prev_hermes_ready.clone() {
                    Some(schema) => {
                        tracing::warn!(
                            "Hermes session schema probe failed ({err}); keeping last-known-good schema"
                        );
                        (HermesSessionSchemaState::Ready(schema), None)
                    }
                    None => (HermesSessionSchemaState::Unavailable(err), Some(Vec::new())),
                };
                match hermes_probe_workspace_root() {
                    Ok(workspace_root) => {
                        match crate::backend::hermes::probe_session_settings_schema(&[
                            workspace_root,
                        ])
                        .await
                        {
                            Ok(probe) => (
                                HermesSessionSchemaState::Ready(probe.schema),
                                Some(probe.profiles),
                            ),
                            Err(err) => {
                                tracing::error!("failed to refresh Hermes session schema: {err}");
                                hermes_schema_or_last_good(err)
                            }
                        }
                    }
                    Err(err) => {
                        tracing::error!("failed to resolve Hermes probe workspace root: {err}");
                        hermes_schema_or_last_good(err)
                    }
                }
            }
        } else {
            (HermesSessionSchemaState::Pending, Some(Vec::new()))
        };

        let mut state = self.state.lock().await;
        state.codex_session_schema = codex_session_schema;
        state.kiro_session_schema = kiro_session_schema;
        state.hermes_session_schema = hermes_session_schema;
        if let Some(hermes_profiles) = hermes_profiles {
            state.hermes_launch_profiles = hermes_profiles;
        }
        fan_out_session_schemas(&mut state, force_emit).await;
        fan_out_backend_config_schemas(&mut state).await;
        fan_out_launch_profile_catalog(&mut state).await;
    }

    async fn refresh_after_backend_setup(&self) {
        let refresh_guard = self.backend_setup_refresh_lock.lock().await;
        for step in BACKEND_SETUP_REFRESH_ORDER {
            match step {
                BackendSetupRefreshStep::Setup => self.fan_out_backend_setup().await,
                BackendSetupRefreshStep::SessionSchemas => {
                    self.refresh_session_schemas_with_fanout(true).await
                }
                BackendSetupRefreshStep::BackendConfigSnapshots => {
                    self.refresh_backend_config_snapshots_with_fanout(true)
                        .await
                }
            }
        }
        drop(refresh_guard);
    }

    pub(crate) async fn run_backend_setup(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: RunBackendSetupPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "run_backend_setup";
        tracing::info!(
            connection_host_stream = %connection_host_stream,
            host_stream = %host_output_stream.path(),
            backend_kind = ?payload.backend_kind,
            action = ?payload.action,
            "host run_backend_setup requested"
        );
        let prepared = setup::prepare_runnable_command(payload.backend_kind, payload.action)
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        let Some(prepared) = prepared else {
            return Err(AppError::not_found(
                OPERATION,
                format!(
                    "no runnable backend setup command for {:?} {:?}",
                    payload.backend_kind, payload.action
                ),
            ));
        };

        let cwd = std::env::current_dir()
            .context("failed to resolve backend setup cwd")
            .map_err(|error| AppError::internal(OPERATION, error))?
            .display()
            .to_string();
        let launch = TerminalLaunchInfo {
            project_id: None,
            root: None,
            cwd,
            cols: 100,
            rows: 28,
            command: TerminalLaunchCommand::Trusted {
                program: prepared.program().to_owned(),
                arguments: prepared.arguments().to_vec(),
            },
        };
        let terminal = self
            .create_resolved_terminal_internal(connection_host_stream, host_output_stream, launch)
            .await?;

        if let Some(terminal) = terminal {
            tracing::info!(
                backend_kind = ?payload.backend_kind,
                action = ?payload.action,
                command = %prepared.display_command(),
                "host run_backend_setup launching terminal command"
            );

            let host = self.clone();
            let backend_kind = payload.backend_kind;
            let action = payload.action;
            tokio::spawn(async move {
                let exit = terminal.wait_for_exit().await;
                drop(prepared);
                tracing::info!(
                    backend_kind = ?backend_kind,
                    action = ?action,
                    exit_code = exit.exit_code,
                    signal = exit.signal.as_deref(),
                    "host run_backend_setup terminal exited; refreshing backend state"
                );
                host.refresh_after_backend_setup().await;
            });
        }
        Ok(())
    }

    pub(crate) async fn create_terminal(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: TerminalCreatePayload,
    ) -> AppResult<()> {
        let _ = self
            .create_terminal_internal(connection_host_stream, host_output_stream, payload)
            .await?;
        Ok(())
    }

    async fn create_terminal_internal(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: TerminalCreatePayload,
    ) -> AppResult<Option<TerminalHandle>> {
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let launch = resolve_terminal_launch(&project_store, payload).await?;
        self.create_resolved_terminal_internal(connection_host_stream, host_output_stream, launch)
            .await
    }

    async fn create_resolved_terminal_internal(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        launch: TerminalLaunchInfo,
    ) -> AppResult<Option<TerminalHandle>> {
        const OPERATION: &str = "terminal_create";
        let launch_project_id = launch.project_id.clone();
        if let Some(project_id) = launch_project_id.as_ref() {
            let state = self.state.lock().await;
            if state.removing_projects.contains(project_id) {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot create terminal in workbench {} because it is being removed",
                        project_id
                    ),
                ));
            }
        }
        let terminal_id = TerminalId(Uuid::new_v4().to_string());
        let terminal_stream_path = StreamPath(format!("/terminal/{}", terminal_id));
        let terminal_output_stream = host_output_stream.with_path(terminal_stream_path.clone());
        let terminal = create_terminal(launch, terminal_output_stream)
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;

        {
            let mut state = self.state.lock().await;
            // Re-check under the same lock that guards `terminal_streams`:
            // workbench removal inserts into `removing_projects` before it
            // validates blockers, so either this terminal is registered in
            // time for the blocker check to see it, or the removal marker
            // is visible here and the terminal is rejected.
            if let Some(project_id) = launch_project_id.as_ref()
                && state.removing_projects.contains(project_id)
            {
                drop(state);
                terminal.close().await;
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "cannot create terminal in workbench {} because it is being removed",
                        project_id
                    ),
                ));
            }
            let previous = state.terminal_streams.insert(
                (connection_host_stream.clone(), terminal_id.clone()),
                terminal.clone(),
            );
            if previous.is_some() {
                return Err(AppError::internal_message(
                    OPERATION,
                    format!(
                        "duplicate terminal registration for {}",
                        terminal_stream_path
                    ),
                    anyhow!("duplicate terminal registration"),
                ));
            }
        }

        let host_payload =
            serde_json::to_value(terminal.new_terminal_payload()).map_err(|error| {
                AppError::internal_message(
                    OPERATION,
                    "failed to serialize new terminal payload",
                    error,
                )
            })?;
        if host_output_stream
            .send_value(FrameKind::NewTerminal, host_payload)
            .is_err()
        {
            self.remove_failed_terminal(connection_host_stream, &terminal_id, &terminal)
                .await;
            return Ok(None);
        }
        if terminal.emit_bootstrap_and_start_io().await.is_err() {
            self.remove_failed_terminal(connection_host_stream, &terminal_id, &terminal)
                .await;
            return Ok(None);
        }
        Ok(Some(terminal))
    }

    async fn remove_failed_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        terminal: &TerminalHandle,
    ) {
        let mut state = self.state.lock().await;
        state
            .terminal_streams
            .remove(&(connection_host_stream.clone(), terminal_id.clone()));
        drop(state);
        terminal.close().await;
    }

    pub(crate) async fn send_terminal_input(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        payload: TerminalSendPayload,
    ) -> AppResult<()> {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await?;
        terminal.send(payload).await;
        Ok(())
    }

    pub(crate) async fn resize_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
        payload: TerminalResizePayload,
    ) -> AppResult<()> {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await?;
        terminal.resize(payload.cols, payload.rows).await;
        Ok(())
    }

    pub(crate) async fn close_terminal(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
    ) -> AppResult<()> {
        let terminal = self
            .terminal_handle(connection_host_stream, terminal_id)
            .await?;
        terminal.close().await;
        Ok(())
    }

    pub(crate) async fn agent_handle(&self, agent_id: &AgentId) -> Option<AgentHandle> {
        self.state.lock().await.registry.agent_handle(agent_id)
    }

    pub(crate) async fn load_agent_stream(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        agent_id: AgentId,
        agent_stream: StreamPath,
    ) -> AppResult<()> {
        let (agent_handle, stream) = {
            let mut state = self.state.lock().await;
            let agent_handle = state.registry.agent_handle(&agent_id).ok_or_else(|| {
                AppError::not_found("load_agent", format!("agent {} is not running", agent_id))
            })?;
            let subscriber = state
                .host_streams
                .get_mut(connection_host_stream)
                .ok_or_else(|| {
                    AppError::not_found(
                        "load_agent",
                        format!("host stream {} is not registered", connection_host_stream),
                    )
                })?;
            if !subscriber.known_agent_streams.contains(&agent_stream) {
                return Err(AppError::not_found(
                    "load_agent",
                    format!(
                        "agent stream {} was not advertised on host stream {}",
                        agent_stream, connection_host_stream
                    ),
                ));
            }
            if subscriber.attached_agent_streams.contains(&agent_stream) {
                return Err(AppError::conflict(
                    "load_agent",
                    format!(
                        "agent stream {} is already attached; reconnect to retry loading the agent",
                        agent_stream
                    ),
                ));
            }
            subscriber
                .attached_agent_streams
                .insert(agent_stream.clone());
            (
                agent_handle,
                host_output_stream.with_path(agent_stream.clone()),
            )
        };

        if agent_handle.attach(stream).await {
            let mut state = self.state.lock().await;
            if let Some(subscriber) = state.host_streams.get_mut(connection_host_stream) {
                subscriber
                    .bootstrapped_agent_streams
                    .insert(agent_stream.clone());
            }
            Ok(())
        } else {
            let mut state = self.state.lock().await;
            if let Some(subscriber) = state.host_streams.get_mut(connection_host_stream) {
                subscriber.attached_agent_streams.remove(&agent_stream);
                subscriber.bootstrapped_agent_streams.remove(&agent_stream);
            }
            Err(AppError::not_found(
                "load_agent",
                format!("agent {} is not running", agent_id),
            ))
        }
    }

    pub(crate) async fn fetch_session_history(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        agent_stream: StreamPath,
        payload: protocol::FetchSessionHistoryPayload,
    ) -> AppResult<()> {
        if payload.limit == 0 {
            return Err(AppError::invalid(
                "fetch_session_history",
                "limit must be greater than zero",
            ));
        }

        let agent_handle = {
            let state = self.state.lock().await;
            let subscriber = state
                .host_streams
                .get(connection_host_stream)
                .ok_or_else(|| {
                    AppError::not_found(
                        "fetch_session_history",
                        format!("host stream {} is not registered", connection_host_stream),
                    )
                })?;
            if !subscriber.known_agent_streams.contains(&agent_stream) {
                return Err(AppError::not_found(
                    "fetch_session_history",
                    format!(
                        "agent stream {} was not advertised on host stream {}",
                        agent_stream, connection_host_stream
                    ),
                ));
            }
            if !subscriber
                .bootstrapped_agent_streams
                .contains(&agent_stream)
            {
                return Err(AppError::invalid(
                    "fetch_session_history",
                    format!(
                        "agent stream {} has not completed AgentBootstrap on host stream {}",
                        agent_stream, connection_host_stream
                    ),
                ));
            }
            state
                .registry
                .agent_handle(&payload.agent_id)
                .ok_or_else(|| {
                    AppError::not_found(
                        "fetch_session_history",
                        format!("agent {} is not running", payload.agent_id),
                    )
                })?
        };

        let Some(window) = agent_handle
            .fetch_session_history(payload.before_seq, payload.limit as usize)
            .await
        else {
            return Err(AppError::not_found(
                "fetch_session_history",
                format!("agent {} is not running", payload.agent_id),
            ));
        };

        let response = SessionHistoryPayload {
            agent_id: payload.agent_id,
            events: window.events,
            has_more_before: window.has_more_before,
            oldest_seq: window.oldest_seq,
        };
        let response = serde_json::to_value(response)
            .map_err(|error| AppError::internal("fetch_session_history", error))?;
        host_output_stream
            .with_path(agent_stream)
            .send_value(FrameKind::SessionHistory, response)
            .map_err(|_| AppError::internal("fetch_session_history", anyhow!("stream closed")))
    }

    pub(crate) async fn interrupt_agent(&self, agent_id: &AgentId) -> InterruptOutcome {
        let agent_handle = {
            let state = self.state.lock().await;
            state.registry.agent_handle(agent_id)
        };

        match agent_handle {
            Some(handle) => handle.interrupt().await,
            None => InterruptOutcome::NotRunning,
        }
    }

    pub(crate) async fn close_agent(&self, agent_id: &AgentId) -> bool {
        self.close_agent_with_host_visibility(agent_id, None).await
    }

    async fn close_agent_for_recorded_visibility(&self, agent_id: &AgentId) -> bool {
        let (close_ids, agent_visibility) = {
            let state = self.state.lock().await;
            (
                state
                    .registry
                    .agent_subtree_post_order(agent_id)
                    .into_iter()
                    .map(|(agent_id, _)| agent_id)
                    .collect::<Vec<_>>(),
                state.agent_visibility.clone(),
            )
        };
        let mut closed = false;
        for close_id in close_ids {
            let visible_host_streams = agent_visibility.visible_host_streams(&close_id);
            closed |= self
                .close_agent_with_host_visibility(&close_id, Some(&visible_host_streams))
                .await;
        }
        closed
    }

    async fn close_agent_with_host_visibility(
        &self,
        agent_id: &AgentId,
        visible_host_streams: Option<&HashSet<StreamPath>>,
    ) -> bool {
        let (close_targets, host_streams) = {
            let state = self.state.lock().await;
            let close_targets = state.registry.agent_subtree_post_order(agent_id);
            if close_targets.is_empty() {
                return false;
            }
            let host_streams = state
                .host_streams
                .iter()
                .filter(|(path, _)| match visible_host_streams {
                    Some(visible) => visible.contains(*path),
                    None => true,
                })
                .map(|(path, subscriber)| {
                    (
                        path.clone(),
                        subscriber.stream.clone(),
                        subscriber.bootstrapped,
                    )
                })
                .collect::<Vec<_>>();
            (close_targets, host_streams)
        };

        let close_ids = close_targets
            .iter()
            .map(|(agent_id, _)| agent_id.clone())
            .collect::<Vec<_>>();
        let bootstrapping_paths = host_streams
            .iter()
            .filter_map(|(path, _, bootstrapped)| (!*bootstrapped).then_some(path.clone()))
            .collect::<Vec<_>>();
        let mut live_streams = host_streams
            .into_iter()
            .filter_map(|(path, stream, bootstrapped)| bootstrapped.then_some((path, stream)))
            .collect::<Vec<_>>();
        let mut dead_paths = Vec::new();
        let mut closed_usage_snapshots = Vec::new();

        for (_, agent_handle) in &close_targets {
            closed_usage_snapshots
                .push(read_agent_usage_snapshot_or_unavailable(agent_handle).await);
        }
        {
            let mut state = self.state.lock().await;
            for snapshot in closed_usage_snapshots {
                state
                    .closed_agent_usage_snapshots
                    .insert(snapshot.start.agent_id.clone(), snapshot);
            }
        }

        for (target_agent_id, agent_handle) in close_targets {
            let _ = agent_handle.close().await;

            let payload = AgentClosedPayload {
                agent_id: target_agent_id,
            };
            if !bootstrapping_paths.is_empty() {
                let payload_value = serde_json::to_value(&payload)
                    .expect("failed to serialize AgentClosed payload for host stream fanout");
                let mut state = self.state.lock().await;
                for path in &bootstrapping_paths {
                    let Some(subscriber) = state.host_streams.get_mut(path) else {
                        continue;
                    };
                    if emit_or_queue_host_frame(
                        subscriber,
                        FrameKind::AgentClosed,
                        payload_value.clone(),
                    )
                    .is_err()
                    {
                        dead_paths.push(path.clone());
                    }
                }
            }
            let mut failed_paths = Vec::new();
            for (path, stream) in &live_streams {
                if emit_agent_closed_for_stream(&payload, stream)
                    .await
                    .is_err()
                {
                    failed_paths.push(path.clone());
                }
            }
            if !failed_paths.is_empty() {
                live_streams.retain(|(path, _)| !failed_paths.contains(path));
                dead_paths.extend(failed_paths);
            }
        }

        dead_paths.sort_by(|left, right| left.0.cmp(&right.0));
        dead_paths.dedup();

        let mut state = self.state.lock().await;
        for path in dead_paths {
            state.host_streams.remove(&path);
            state.agent_visibility.remove_host_stream(&path);
        }

        for closed_agent_id in close_ids {
            let removed = state.registry.remove_agent(&closed_agent_id);
            state.agent_visibility.remove_agent(&closed_agent_id);
            if removed.is_none() {
                tracing::debug!(
                    agent_id = %closed_agent_id,
                    "agent was already removed before close cleanup completed"
                );
                continue;
            }

            let removed_session_id = state.agent_sessions.remove(&closed_agent_id);
            let removed_pending_session_id = state.pending_agent_sessions.remove(&closed_agent_id);
            state.agent_activity_summaries.remove(&closed_agent_id);
            for subscriber in state.host_streams.values_mut() {
                forget_agent_fanout_for_subscriber(subscriber, &closed_agent_id);
            }
            let snapshot_session_id = removed
                .as_ref()
                .and_then(|agent| agent.snapshot().session_id);
            if (removed_pending_session_id.is_some()
                || removed_session_id.or(snapshot_session_id).is_none())
                && let Some(store) = state.agents_view_preferences_store.clone()
                && let Err(error) = store.lock().await.remove_transient_agent(
                    HostFilterId(LOCAL_HOST_ID.to_owned()),
                    closed_agent_id.clone(),
                )
            {
                tracing::warn!(
                    agent_id = %closed_agent_id,
                    error = %error,
                    "failed to remove transient agent annotations during close cleanup"
                );
            }
            match state
                .team_registry
                .clear_binding_by_agent(closed_agent_id.clone())
                .await
            {
                Ok(events) => fan_out_team_registry_events(&mut state, events).await,
                Err(error) => {
                    tracing::warn!(
                        agent_id = %closed_agent_id,
                        error = %error,
                        "failed to clear team binding while closing agent"
                    );
                }
            }
        }
        fan_out_session_lists(&mut state).await;
        fan_out_current_agents_view_preferences(&mut state).await;
        drop(state);
        self.fan_out_task_token_usages().await;

        true
    }

    pub(crate) async fn agent_status_handle(
        &self,
        agent_id: &AgentId,
    ) -> Option<crate::agent::registry::AgentStatusHandle> {
        self.state
            .lock()
            .await
            .registry
            .agent_status_handle(agent_id)
    }

    pub(crate) async fn agent_access_mode(
        &self,
        agent_id: &AgentId,
    ) -> Option<protocol::BackendAccessMode> {
        self.state.lock().await.registry.agent_access_mode(agent_id)
    }

    pub(crate) async fn agent_status_snapshot(
        &self,
        agent_id: &AgentId,
    ) -> Option<crate::agent::registry::AgentStatus> {
        let handle = self
            .state
            .lock()
            .await
            .registry
            .agent_status_handle(agent_id)?;
        Some(handle.snapshot().await)
    }

    pub(crate) async fn list_agents(&self) -> Vec<protocol::AgentStartPayload> {
        let handles = {
            let state = self.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .filter_map(|agent_id| state.registry.agent_handle(&agent_id))
                .collect::<Vec<_>>()
        };

        let mut starts = Vec::with_capacity(handles.len());
        for handle in handles {
            starts.push(handle.snapshot());
        }
        starts
    }

    async fn agent_project_id(&self, agent_id: &AgentId) -> Option<ProjectId> {
        let handle = self.state.lock().await.registry.agent_handle(agent_id)?;
        handle.snapshot().project_id
    }

    pub async fn agent_ids(&self) -> Vec<AgentId> {
        self.state.lock().await.registry.agent_ids()
    }

    pub(crate) async fn subscribe_agent_status_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.state.lock().await.registry.subscribe_status_changes()
    }

    pub(crate) async fn read_settings(&self) -> Result<protocol::HostSettings, String> {
        self.state.lock().await.settings_store.lock().await.get()
    }

    pub(crate) async fn read_launch_profile_catalog(&self) -> Result<LaunchProfileCatalog, String> {
        let state = self.state.lock().await;
        let settings = state.settings_store.lock().await.get()?;
        Ok(launch_profile_catalog_for_settings(&state, &settings))
    }

    pub(crate) async fn read_launch_options(
        &self,
    ) -> Result<
        (
            LaunchProfileCatalog,
            Option<BackendKind>,
            Vec<SessionSchemaEntry>,
        ),
        String,
    > {
        let state = self.state.lock().await;
        let settings = state.settings_store.lock().await.get()?;
        let catalog = launch_profile_catalog_for_settings(&state, &settings);
        let session_schemas =
            session_schemas_for_enabled_backends(&state, &settings.enabled_backends);
        Ok((catalog, settings.default_backend, session_schemas))
    }

    pub(crate) async fn resolve_launch_profile(
        &self,
        launch_profile_id: &LaunchProfileId,
    ) -> Result<LaunchProfile, String> {
        let catalog = self.read_launch_profile_catalog().await?;
        resolve_launch_profile_from_catalog(&catalog, launch_profile_id)
    }

    async fn activity_summary_settings_receiver(
        &self,
    ) -> watch::Receiver<ActivitySummarySettingsSignal> {
        self.state
            .lock()
            .await
            .activity_summary_settings_tx
            .subscribe()
    }

    async fn supervisor_settings_receiver(&self) -> watch::Receiver<SupervisorSettingsSignal> {
        self.state.lock().await.supervisor_settings_tx.subscribe()
    }

    async fn supervisor_settings_signal(&self) -> SupervisorSettingsSignal {
        *self.state.lock().await.supervisor_settings_tx.borrow()
    }

    async fn activity_summary_observations(&self) -> Vec<ActivitySummaryObservation> {
        let entries = {
            let state = self.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .filter_map(|agent_id| {
                    let handle = state.registry.agent_handle(&agent_id)?;
                    let status_handle = state.registry.agent_status_handle(&agent_id)?;
                    Some((agent_id, handle, status_handle))
                })
                .collect::<Vec<_>>()
        };

        let mut observations = Vec::with_capacity(entries.len());
        for (agent_id, handle, status_handle) in entries {
            let status = status_handle.snapshot().await;
            let start = handle.snapshot();
            observations.push(ActivitySummaryObservation {
                agent_id,
                handle,
                start,
                status,
            });
        }
        observations
    }

    async fn activity_summary_observation(
        &self,
        agent_id: &AgentId,
    ) -> Option<ActivitySummaryObservation> {
        let (handle, status_handle) = {
            let state = self.state.lock().await;
            (
                state.registry.agent_handle(agent_id)?,
                state.registry.agent_status_handle(agent_id)?,
            )
        };
        let status = status_handle.snapshot().await;
        let start = handle.snapshot();
        Some(ActivitySummaryObservation {
            agent_id: agent_id.clone(),
            handle,
            start,
            status,
        })
    }

    async fn use_mock_backend(&self) -> bool {
        self.state.lock().await.use_mock_backend
    }

    async fn is_agent_registered(&self, agent_id: &AgentId) -> bool {
        self.state
            .lock()
            .await
            .registry
            .agent_handle(agent_id)
            .is_some()
    }

    async fn agent_activity_summary_for_context(
        &self,
        agent_id: &AgentId,
    ) -> Option<AgentActivitySummary> {
        let state = self.state.lock().await;
        state
            .agent_activity_summaries
            .get(agent_id)
            .and_then(activity_summary_from_state)
    }

    async fn set_agent_activity_summary_empty(&self, agent_id: AgentId) {
        self.set_agent_activity_summary_state(agent_id, AgentActivitySummaryState::Empty)
            .await;
    }

    async fn set_agent_activity_summary_error(&self, agent_id: AgentId, message: String) {
        let previous = self.agent_activity_summary_for_context(&agent_id).await;
        self.set_agent_activity_summary_state(
            agent_id,
            AgentActivitySummaryState::Error {
                message,
                occurred_at_ms: crate::agent::now_ms(),
                previous,
            },
        )
        .await;
    }

    async fn mark_agent_activity_summary_stale_if_fresh(&self, agent_id: AgentId) {
        let state_to_emit = {
            let mut state = self.state.lock().await;
            let Some(AgentActivitySummaryState::Fresh { summary }) =
                state.agent_activity_summaries.get(&agent_id).cloned()
            else {
                return;
            };
            let stale = AgentActivitySummaryState::Stale {
                summary,
                reason: AgentActivitySummaryStaleReason::NewActivity,
            };
            state
                .agent_activity_summaries
                .insert(agent_id.clone(), stale.clone());
            stale
        };
        self.fan_out_agent_activity_summary(agent_id, state_to_emit)
            .await;
    }

    async fn set_agent_activity_summary_state(
        &self,
        agent_id: AgentId,
        summary_state: AgentActivitySummaryState,
    ) {
        let changed = {
            let mut state = self.state.lock().await;
            if state.agent_activity_summaries.get(&agent_id) == Some(&summary_state) {
                false
            } else {
                state
                    .agent_activity_summaries
                    .insert(agent_id.clone(), summary_state.clone());
                true
            }
        };
        if changed {
            self.fan_out_agent_activity_summary(agent_id, summary_state)
                .await;
        }
    }

    async fn fan_out_agent_activity_summary(
        &self,
        agent_id: AgentId,
        summary_state: AgentActivitySummaryState,
    ) {
        let mut state = self.state.lock().await;
        fan_out_agent_activity_summary(
            &mut state,
            AgentActivitySummaryPayload {
                agent_id,
                state: summary_state,
            },
        )
        .await;
    }

    pub async fn agent_control_mcp_url(&self) -> String {
        self.state.lock().await.agent_control_mcp.url.clone()
    }

    pub async fn agent_control_mcp_caller(
        &self,
        agent_id: &AgentId,
    ) -> Result<crate::agent_control_mcp::AgentControlMcpCaller, String> {
        let state = self.state.lock().await;
        if state.registry.agent_handle(agent_id).is_none() {
            return Err(format!("unknown agent_id {}", agent_id.0));
        }
        Ok(state.agent_control_mcp.caller(agent_id))
    }

    pub async fn review_mcp_url(&self) -> String {
        self.state.lock().await.review_mcp.url.clone()
    }

    pub async fn workflow_mcp_url(&self) -> String {
        self.state.lock().await.workflow_mcp.url.clone()
    }

    pub(crate) async fn propose_review_comment(
        &self,
        review_id: ReviewId,
        suggestion: protocol::ReviewSuggestedComment,
    ) -> Result<Result<protocol::ReviewSuggestionId, protocol::ReviewErrorPayload>, String> {
        let registry = {
            let state = self.state.lock().await;
            state.review_registry.clone()
        };
        registry.ai_suggestion(review_id, suggestion).await
    }

    /// Spawn an agent from agent-control MCP, inheriting workflow context and
    /// project ownership from the calling or parent agent when applicable.
    pub(crate) async fn spawn_agent_from_agent_control(
        &self,
        mut payload: SpawnAgentPayload,
        caller_agent_id: Option<&AgentId>,
    ) -> Result<AgentId, String> {
        let workflow = if let Some(caller_agent_id) = caller_agent_id {
            self.workflow_metadata_for_agent(caller_agent_id).await
        } else {
            None
        };
        let inherited_project_source = if payload.project_id.is_none() {
            if workflow.is_some() {
                caller_agent_id.cloned()
            } else {
                payload
                    .parent_agent_id
                    .clone()
                    .or_else(|| caller_agent_id.cloned())
            }
        } else {
            None
        };
        if let Some(source_agent_id) = inherited_project_source
            && let Some(project_id) = self.agent_project_id(&source_agent_id).await
        {
            payload.project_id = Some(project_id);
        }
        let project_parent_lock = if let Some(project_id) = payload.project_id.as_ref() {
            let project = self
                .list_projects()
                .await?
                .into_iter()
                .find(|project| &project.id == project_id)
                .ok_or_else(|| format!("cannot spawn agent in missing project {project_id}"))?;
            match project.source {
                ProjectSource::GitWorkbench {
                    parent_project_id, ..
                } => Some(self.workbench_parent_lock(&parent_project_id).await),
                ProjectSource::Standalone { .. } => None,
            }
        } else {
            None
        };
        let _project_parent_guard = match project_parent_lock.as_ref() {
            Some(lock) => {
                notify_workbench_spawn_waiting_test_hook(self);
                Some(lock.lock().await)
            }
            None => None,
        };
        let requested_roots = match &payload.params {
            SpawnAgentParams::New {
                workspace_roots, ..
            } => workspace_roots.clone(),
            SpawnAgentParams::Resume { .. } | SpawnAgentParams::Fork { .. } => {
                return Err("agent-control MCP spawn must use kind=new".to_owned());
            }
        };
        let workspace_roots = self
            .resolve_spawn_workspace_roots(payload.project_id.as_ref(), &requested_roots)
            .await?;
        let SpawnAgentParams::New {
            workspace_roots: roots,
            ..
        } = &mut payload.params
        else {
            return Err("agent-control MCP spawn must use kind=new".to_owned());
        };
        *roots = workspace_roots;
        if let Some(workflow) = workflow {
            let backend_kind = match &payload.params {
                SpawnAgentParams::New { backend_kind, .. } => *backend_kind,
                SpawnAgentParams::Resume { .. } | SpawnAgentParams::Fork { .. } => {
                    return Err(
                        "workflow child agents spawned through MCP must use kind=new".to_owned(),
                    );
                }
            };
            self.ensure_workflow_backend_declared(&workflow, backend_kind)
                .await?;
            if let Some(caller_agent_id) = caller_agent_id {
                payload.parent_agent_id = Some(caller_agent_id.clone());
            }
            let agent_id = self
                .spawn_agent_with_origin_and_config(
                    payload,
                    AgentOrigin::Workflow,
                    None,
                    Some(workflow.clone()),
                )
                .await
                .map_err(|error| error.to_string())?;
            self.workflow_note_agent(&workflow.workflow_run_id, agent_id.clone(), false)
                .await?;
            return Ok(agent_id);
        }

        self.spawn_agent_with_origin(payload, AgentOrigin::AgentControl)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn resolve_spawn_workspace_roots(
        &self,
        project_id: Option<&ProjectId>,
        requested: &[String],
    ) -> Result<Vec<String>, String> {
        let Some(project_id) = project_id else {
            if requested.is_empty() {
                return Err(
                    "workspace_roots must contain at least one root when project_id is omitted"
                        .to_owned(),
                );
            }
            return Ok(requested.to_vec());
        };
        let state = self.state.lock().await;
        if state.removing_projects.contains(project_id) {
            return Err(format!(
                "cannot spawn agent in workbench {project_id} because it is being removed"
            ));
        }
        let project = state
            .project_store
            .lock()
            .await
            .list()?
            .into_iter()
            .find(|project| &project.id == project_id)
            .ok_or_else(|| format!("cannot spawn agent in missing project {project_id}"))?;
        let authoritative = project
            .root_paths()
            .into_iter()
            .map(|root| root.0)
            .collect::<Vec<_>>();
        if requested.is_empty() {
            return Ok(authoritative);
        }
        let requested_set = requested.iter().collect::<HashSet<_>>();
        let authoritative_set = authoritative.iter().collect::<HashSet<_>>();
        if requested_set != authoritative_set {
            return Err(format!(
                "workspace_roots {:?} do not match authoritative roots {:?} for project {}",
                requested, authoritative, project_id
            ));
        }
        Ok(authoritative)
    }

    async fn workflow_metadata_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> Option<AgentWorkflowMetadata> {
        self.state
            .lock()
            .await
            .registry
            .agent_handle(agent_id)
            .map(|handle| handle.snapshot().workflow)
            .unwrap_or(None)
    }

    async fn ensure_workflow_backend_declared(
        &self,
        workflow: &AgentWorkflowMetadata,
        backend_kind: protocol::BackendKind,
    ) -> Result<(), String> {
        let declared = {
            let state = self.state.lock().await;
            let run = state
                .workflow_run_store
                .get(&workflow.workflow_run_id)
                .ok_or_else(|| format!("unknown workflow run {}", workflow.workflow_run_id))?;
            state
                .workflow_catalog
                .resolve(&workflow.workflow_id, run.project_id.as_ref())
                .map(|definition| definition.summary.declared_backends)
        }
        .ok_or_else(|| format!("unknown workflow_id {}", workflow.workflow_id))?;
        if declared.contains(&backend_kind) {
            Ok(())
        } else {
            Err(format!(
                "workflow {} did not declare backend {:?}",
                workflow.workflow_id, backend_kind
            ))
        }
    }

    pub(crate) async fn refresh_workflows(&self) -> AppResult<()> {
        self.reload_workflows_and_notify("workflow_refresh").await
    }

    async fn reload_workflows_and_notify(&self, reason: &'static str) -> AppResult<()> {
        self.reload_workflows_and_notify_inner(reason, None).await
    }

    async fn reload_workflows_and_notify_inner(
        &self,
        reason: &'static str,
        extra_diagnostic: Option<WorkflowDiagnostic>,
    ) -> AppResult<()> {
        let mut state = self.state.lock().await;
        let projects = state.project_store.lock().await.list().map_err(|error| {
            AppError::internal_message(reason, error.to_string(), anyhow!(error.to_string()))
        })?;
        let mut catalog = WorkflowCatalog::discover(&projects);
        if let Some(diagnostic) = extra_diagnostic {
            catalog.push_diagnostic(diagnostic);
        }
        let locations = workflow_catalog_locations(&projects);
        let payload = WorkflowNotifyPayload {
            summaries: catalog.summaries(),
            diagnostics: catalog.diagnostics(),
            locations: locations.clone(),
        };
        state.workflow_catalog = catalog;
        state.workflow_locations = locations;
        fan_out_workflow_notify(&mut state, payload).await;
        Ok(())
    }

    async fn update_workflow_watcher_targets(&self) -> AppResult<()> {
        let (watcher, targets) = {
            let state = self.state.lock().await;
            let projects = state.project_store.lock().await.list().map_err(|error| {
                AppError::internal_message(
                    "workflow_watch_targets",
                    error.to_string(),
                    anyhow!(error.to_string()),
                )
            })?;
            (
                state.workflow_watcher.clone(),
                workflow_watch_dirs(&projects),
            )
        };
        watcher.set_targets(targets).await.map_err(|error| {
            AppError::internal_message("workflow_watch_targets", error.clone(), anyhow!(error))
        })
    }

    async fn update_workflow_watcher_targets_and_reload(
        &self,
        reason: &'static str,
    ) -> AppResult<()> {
        self.update_workflow_watcher_targets().await?;
        self.reload_workflows_and_notify(reason).await
    }

    pub(crate) async fn workflow_targets_for_agent(
        &self,
        caller_agent_id: Option<&AgentId>,
    ) -> Result<WorkflowTargetsResponse, String> {
        let state = self.state.lock().await;
        let projects = state.project_store.lock().await.list()?;
        let mut target_projects = Vec::new();
        match caller_agent_id {
            Some(agent_id) => {
                let handle = state
                    .registry
                    .agent_handle(agent_id)
                    .ok_or_else(|| format!("unknown caller agent_id {agent_id}"))?;
                if let Some(project_id) = handle.snapshot().project_id {
                    let project = projects
                        .iter()
                        .find(|project| project.id == project_id)
                        .ok_or_else(|| format!("caller project {project_id} no longer exists"))?;
                    target_projects.push(project.clone());
                }
            }
            None => target_projects.extend(projects.clone()),
        }
        drop(state);

        let mut targets = vec![WorkflowTargetDirectory {
            target: WorkflowSaveTarget::Global,
            location: workflow_location_for_scope(WorkflowSourceScope::Global),
        }];
        for project in target_projects {
            for root in project.root_paths() {
                let scope = WorkflowSourceScope::Project {
                    project_id: project.id.clone(),
                    root: root.clone(),
                };
                targets.push(WorkflowTargetDirectory {
                    target: WorkflowSaveTarget::Project {
                        project_id: project.id.clone(),
                        root,
                    },
                    location: workflow_location_for_scope(scope),
                });
            }
        }
        Ok(WorkflowTargetsResponse { targets })
    }

    pub(crate) async fn workflow_save_from_agent(
        &self,
        request: WorkflowSaveRequest,
    ) -> Result<WorkflowSaveResponse, String> {
        let save_guard = self.workflow_save_lock.lock().await;
        let (target_dir, scope) = self.resolve_workflow_save_target(&request.target).await?;
        let filename = validate_workflow_filename(&request.filename)?;
        let path = target_dir.join(filename);
        let path_string = path.display().to_string();
        let source = WorkflowSource {
            scope: scope.clone(),
            path: path_string.clone(),
        };
        let replacement = parse_workflow_content(&request.markdown, source.clone())
            .map_err(|error| error.message().to_owned())?;

        let created = match &request.mode {
            WorkflowSaveMode::Create => {
                if path.exists() {
                    return Err(format!("workflow file already exists: {path_string}"));
                }
                let fresh_catalog = self.fresh_workflow_catalog_for_save().await?;
                if fresh_catalog.has_same_scope_id(&scope, &replacement.summary.id) {
                    return Err(format!(
                        "workflow id {} already exists in the same scope",
                        replacement.summary.id
                    ));
                }
                true
            }
            WorkflowSaveMode::Replace {
                existing_path,
                existing_id,
            } => {
                if !path.exists() {
                    return Err(format!("workflow file does not exist: {path_string}"));
                }
                if existing_path != &path_string {
                    return Err(format!(
                        "replace existing_path {existing_path:?} does not match target path {path_string:?}"
                    ));
                }
                let current = parse_workflow_file(&path, source.clone())
                    .map_err(|error| error.message().to_owned())?;
                if &current.summary.id != existing_id {
                    return Err(format!(
                        "replace existing_id {} does not match current workflow id {}",
                        existing_id, current.summary.id
                    ));
                }
                if replacement.summary.id != *existing_id {
                    return Err(
                        "replace cannot change workflow id; create a new workflow instead"
                            .to_owned(),
                    );
                }
                false
            }
        };

        atomic_write_workflow(&path, request.markdown.as_bytes(), !created)?;
        self.update_workflow_watcher_targets()
            .await
            .map_err(|error| error.to_string())?;
        self.reload_workflows_and_notify("workflow_save")
            .await
            .map_err(|error| error.to_string())?;

        let state = self.state.lock().await;
        let summary = state
            .workflow_catalog
            .summary_for_path(&path_string)
            .ok_or_else(|| {
                format!("saved workflow {path_string} was not present after workflow reload")
            })?;
        let diagnostics = state.workflow_catalog.diagnostics_for_path(&path_string);
        let response = WorkflowSaveResponse {
            source: summary.source.clone(),
            summary,
            path: path_string,
            created,
            diagnostics,
        };
        drop(save_guard);
        Ok(response)
    }

    async fn fresh_workflow_catalog_for_save(&self) -> Result<WorkflowCatalog, String> {
        let state = self.state.lock().await;
        let projects = state.project_store.lock().await.list()?;
        Ok(WorkflowCatalog::discover(&projects))
    }

    async fn resolve_workflow_save_target(
        &self,
        target: &WorkflowSaveTarget,
    ) -> Result<(PathBuf, WorkflowSourceScope), String> {
        match target {
            WorkflowSaveTarget::Global => Ok((global_workflows_dir(), WorkflowSourceScope::Global)),
            WorkflowSaveTarget::Project { project_id, root } => {
                let state = self.state.lock().await;
                let project = state
                    .project_store
                    .lock()
                    .await
                    .get(project_id)
                    .ok_or_else(|| format!("unknown project_id {project_id}"))?;
                if !project
                    .root_paths()
                    .iter()
                    .any(|candidate| candidate == root)
                {
                    return Err(format!(
                        "root {} does not belong to project {}",
                        root, project_id
                    ));
                }
                Ok((
                    project_workflows_dir(root),
                    WorkflowSourceScope::Project {
                        project_id: project_id.clone(),
                        root: root.clone(),
                    },
                ))
            }
        }
    }

    pub(crate) async fn trigger_workflow(&self, payload: TriggerWorkflowPayload) -> AppResult<()> {
        const OPERATION: &str = "trigger_workflow";
        let workflow_id = payload.workflow_id;
        let project_id = payload.project_id;
        let (definition, workspace_roots, workflow_mcp, settings, use_mock_backend) = {
            let state = self.state.lock().await;
            let definition = state
                .workflow_catalog
                .resolve(&workflow_id, project_id.as_ref())
                .ok_or_else(|| {
                    AppError::not_found(OPERATION, format!("unknown workflow {workflow_id}"))
                })?;
            let workspace_roots = if let Some(project_id) = project_id.as_ref() {
                let project = state
                    .project_store
                    .lock()
                    .await
                    .get(project_id)
                    .ok_or_else(|| {
                        AppError::not_found(OPERATION, format!("unknown project {project_id}"))
                    })?;
                project
                    .root_paths()
                    .into_iter()
                    .map(|root| root.0)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let settings = state.settings_store.lock().await.get().map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
            (
                definition,
                workspace_roots,
                state.workflow_mcp.clone(),
                settings,
                state.use_mock_backend,
            )
        };
        let inputs = validate_workflow_trigger_inputs(
            OPERATION,
            &definition.summary.inputs,
            payload.inputs,
            &workflow_id,
        )?;

        let now = crate::agent::now_ms();
        let run_id = WorkflowRunId(Uuid::new_v4().to_string());
        let run = WorkflowRunSnapshot {
            id: run_id.clone(),
            workflow_id: definition.summary.id.clone(),
            workflow_name: definition.summary.name.clone(),
            source: definition.summary.source.clone(),
            project_id: project_id.clone(),
            coordinator_agent_id: None,
            coordinator: definition.summary.coordinator.clone(),
            status: WorkflowRunSnapshotStatus::Running,
            inputs: inputs.clone(),
            steps: Vec::new(),
            agent_ids: Vec::new(),
            summary: None,
            error: None,
            created_at_ms: now,
            updated_at_ms: now,
            completed_at_ms: None,
        };
        {
            let mut state = self.state.lock().await;
            state
                .workflow_run_store
                .upsert(run.clone())
                .map_err(|error| {
                    AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
                })?;
            fan_out_workflow_run_notify(&mut state, run).await;
        }

        let prompt = crate::workflows::runner::build_coordinator_prompt(
            &run_id,
            &definition.summary,
            &definition.body,
            &inputs,
        );
        let workflow_metadata = AgentWorkflowMetadata {
            workflow_id: definition.summary.id.clone(),
            workflow_run_id: run_id.clone(),
        };
        let mut startup_mcp_servers = {
            let state = self.state.lock().await;
            startup_mcp_servers_for_settings(
                &settings,
                &workspace_roots,
                &state.debug_mcp,
                &state.agent_control_mcp,
                &state.config_mcp,
                None,
            )
        };
        if !workflow_mcp.url.is_empty() {
            startup_mcp_servers.push(StartupMcpServer {
                name: WORKFLOW_PROGRESS_MCP_SERVER_NAME.to_owned(),
                transport: StartupMcpTransport::Http {
                    url: workflow_mcp.url.clone(),
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            });
        }
        let resolved_spawn_config = ResolvedSpawnConfig {
            access_mode: definition.summary.coordinator.access_mode,
            ..Default::default()
        };
        let request = ResolvedSpawnRequest {
            name: format!("Workflow: {}", definition.summary.name),
            origin: AgentOrigin::Workflow,
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            workflow: Some(workflow_metadata),
            parent_agent_id: None,
            parent_session_id: None,
            project_id,
            backend_kind: definition.summary.coordinator.backend,
            launch_profile_id: None,
            workspace_roots,
            initial_input: Some(SendMessagePayload {
                message: prompt,
                images: None,
                origin: None,
                tool_response: None,
            }),
            cost_hint: None,
            session_settings: None,
            session_settings_schema: None,
            backend_config: Default::default(),
            startup_mcp_servers,
            resolved_spawn_config,
            resume_session_id: None,
            fork_from_session_id: None,
            startup_warning: None,
            startup_failure: None,
            initial_alias: Some(InitialAgentAlias {
                name: format!("Workflow: {}", definition.summary.name),
                persistence: InitialAgentAliasPersistence::User,
            }),
            use_mock_backend,
        };
        let coordinator_agent_id = self.spawn_resolved_agent(request).await;
        self.workflow_note_agent(&run_id, coordinator_agent_id.clone(), true)
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        self.spawn_workflow_completion_watcher(run_id, coordinator_agent_id);
        Ok(())
    }

    pub(crate) async fn cancel_workflow(&self, payload: CancelWorkflowPayload) -> AppResult<()> {
        const OPERATION: &str = "cancel_workflow";
        let run = {
            let state = self.state.lock().await;
            state
                .workflow_run_store
                .get(&payload.run_id)
                .ok_or_else(|| {
                    AppError::not_found(
                        OPERATION,
                        format!("unknown workflow run {}", payload.run_id),
                    )
                })?
        };
        if is_workflow_terminal(run.status) {
            return Ok(());
        }
        let mut interrupt_ids = run.agent_ids.clone();
        if let Some(coordinator) = run.coordinator_agent_id.as_ref() {
            let subtree = {
                let state = self.state.lock().await;
                state.registry.agent_subtree_post_order(coordinator)
            };
            interrupt_ids.extend(subtree.into_iter().map(|(id, _)| id));
        }
        interrupt_ids.sort_by(|left, right| left.0.cmp(&right.0));
        interrupt_ids.dedup();
        for agent_id in interrupt_ids {
            let _ = self.interrupt_agent(&agent_id).await;
        }
        self.workflow_update_run_allow_terminal(payload.run_id, |run| {
            if is_workflow_terminal(run.status) {
                return;
            }
            let now = crate::agent::now_ms();
            run.status = WorkflowRunSnapshotStatus::Cancelled;
            run.error = Some("Workflow cancelled by user".to_owned());
            run.updated_at_ms = now;
            run.completed_at_ms = Some(now);
        })
        .await
        .map_err(|error| AppError::internal_message(OPERATION, error.clone(), anyhow!(error)))?;
        Ok(())
    }

    pub(crate) async fn workflow_report_step(
        &self,
        caller_agent_id: AgentId,
        input: WorkflowReportStepToolInput,
    ) -> Result<WorkflowRunSnapshot, String> {
        let metadata = self
            .workflow_metadata_for_agent(&caller_agent_id)
            .await
            .ok_or_else(|| "calling agent is not part of a workflow".to_owned())?;
        let status = crate::workflows::mcp::parse_step_status(input.status.as_deref())?;
        let referenced_agent = input
            .agent_id
            .as_deref()
            .map(parse_workflow_agent_id)
            .transpose()?;
        if let Some(agent_id) = referenced_agent.as_ref() {
            self.ensure_agent_belongs_to_workflow(&metadata.workflow_run_id, agent_id)
                .await?;
        }
        let step_id = input
            .step_id
            .filter(|value| !value.trim().is_empty())
            .map(WorkflowStepRunId)
            .unwrap_or_else(|| WorkflowStepRunId(Uuid::new_v4().to_string()));
        let parent_step_id = input
            .parent_step_id
            .filter(|value| !value.trim().is_empty())
            .map(WorkflowStepRunId);
        let title = input
            .title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Workflow step".to_owned());
        self.workflow_update_run(metadata.workflow_run_id, move |run| {
            let now = crate::agent::now_ms();
            if let Some(step) = run.steps.iter_mut().find(|step| step.id == step_id) {
                step.title = title.clone();
                step.status = status;
                step.agent_id = referenced_agent.clone();
                step.message = input.message.clone();
                step.updated_at_ms = now;
                if matches!(
                    status,
                    protocol::WorkflowStepRunSnapshotStatus::Completed
                        | protocol::WorkflowStepRunSnapshotStatus::Failed
                        | protocol::WorkflowStepRunSnapshotStatus::Cancelled
                ) {
                    step.completed_at_ms = Some(now);
                }
            } else {
                run.steps.push(WorkflowStepRunSnapshot {
                    id: step_id.clone(),
                    parent_step_id: parent_step_id.clone(),
                    title: title.clone(),
                    status,
                    agent_id: referenced_agent.clone(),
                    message: input.message.clone(),
                    created_at_ms: now,
                    updated_at_ms: now,
                    completed_at_ms: if matches!(
                        status,
                        protocol::WorkflowStepRunSnapshotStatus::Completed
                            | protocol::WorkflowStepRunSnapshotStatus::Failed
                            | protocol::WorkflowStepRunSnapshotStatus::Cancelled
                    ) {
                        Some(now)
                    } else {
                        None
                    },
                });
            }
            run.updated_at_ms = now;
        })
        .await
    }

    pub(crate) async fn workflow_finish(
        &self,
        caller_agent_id: AgentId,
        input: WorkflowFinishToolInput,
    ) -> Result<WorkflowRunSnapshot, String> {
        let metadata = self
            .workflow_metadata_for_agent(&caller_agent_id)
            .await
            .ok_or_else(|| "calling agent is not part of a workflow".to_owned())?;
        let status = match input.status.as_deref().map(str::trim) {
            Some("completed" | "complete" | "success" | "succeeded") => {
                WorkflowRunSnapshotStatus::Completed
            }
            Some("failed" | "error") => WorkflowRunSnapshotStatus::Failed,
            Some("cancelled" | "canceled") => WorkflowRunSnapshotStatus::Cancelled,
            Some(other) => return Err(format!("unknown workflow finish status {other:?}")),
            None if input.success == Some(false) => WorkflowRunSnapshotStatus::Failed,
            None => WorkflowRunSnapshotStatus::Completed,
        };
        self.workflow_update_run(metadata.workflow_run_id, move |run| {
            let now = crate::agent::now_ms();
            run.status = status;
            run.summary = input.summary.clone();
            run.error = input.error.clone();
            run.updated_at_ms = now;
            run.completed_at_ms = Some(now);
        })
        .await
    }

    async fn workflow_note_agent(
        &self,
        run_id: &WorkflowRunId,
        agent_id: AgentId,
        coordinator: bool,
    ) -> Result<WorkflowRunSnapshot, String> {
        let run_id = run_id.clone();
        self.workflow_update_run_allow_terminal(run_id, move |run| {
            if coordinator {
                run.coordinator_agent_id = Some(agent_id.clone());
            }
            if !run.agent_ids.contains(&agent_id) {
                run.agent_ids.push(agent_id);
            }
            run.updated_at_ms = crate::agent::now_ms();
        })
        .await
    }

    async fn workflow_update_run<F>(
        &self,
        run_id: WorkflowRunId,
        update: F,
    ) -> Result<WorkflowRunSnapshot, String>
    where
        F: FnOnce(&mut WorkflowRunSnapshot),
    {
        self.workflow_update_run_inner(run_id, false, update).await
    }

    async fn workflow_update_run_allow_terminal<F>(
        &self,
        run_id: WorkflowRunId,
        update: F,
    ) -> Result<WorkflowRunSnapshot, String>
    where
        F: FnOnce(&mut WorkflowRunSnapshot),
    {
        self.workflow_update_run_inner(run_id, true, update).await
    }

    async fn workflow_update_run_inner<F>(
        &self,
        run_id: WorkflowRunId,
        allow_terminal_update: bool,
        update: F,
    ) -> Result<WorkflowRunSnapshot, String>
    where
        F: FnOnce(&mut WorkflowRunSnapshot),
    {
        let mut state = self.state.lock().await;
        let mut run = state
            .workflow_run_store
            .get(&run_id)
            .ok_or_else(|| format!("unknown workflow run {run_id}"))?;
        if !allow_terminal_update && is_workflow_terminal(run.status) {
            return Err(format!(
                "workflow run {run_id} is already {}",
                workflow_status_label(run.status)
            ));
        }
        update(&mut run);
        state.workflow_run_store.upsert(run.clone())?;
        fan_out_workflow_run_notify(&mut state, run.clone()).await;
        Ok(run)
    }

    async fn ensure_agent_belongs_to_workflow(
        &self,
        run_id: &WorkflowRunId,
        agent_id: &AgentId,
    ) -> Result<(), String> {
        let run = {
            let state = self.state.lock().await;
            state
                .workflow_run_store
                .get(run_id)
                .ok_or_else(|| format!("unknown workflow run {run_id}"))?
        };
        if run.agent_ids.contains(agent_id) {
            return Ok(());
        }
        if let Some(coordinator) = run.coordinator_agent_id.as_ref() {
            let subtree = {
                let state = self.state.lock().await;
                state.registry.agent_subtree_post_order(coordinator)
            };
            if subtree.iter().any(|(id, _)| id == agent_id) {
                return Ok(());
            }
        }
        Err(format!(
            "agent_id {agent_id} is not associated with workflow run {run_id}"
        ))
    }

    fn spawn_workflow_completion_watcher(
        &self,
        run_id: WorkflowRunId,
        coordinator_agent_id: AgentId,
    ) {
        let host = self.clone();
        tokio::spawn(async move {
            let mut status_rx = host.subscribe_agent_status_changes().await;
            loop {
                let run_status = {
                    let state = host.state.lock().await;
                    state.workflow_run_store.get(&run_id).map(|run| run.status)
                };
                let Some(run_status) = run_status else {
                    return;
                };
                if is_workflow_terminal(run_status) {
                    return;
                }
                match host.agent_status_snapshot(&coordinator_agent_id).await {
                    Some(status) if status.terminated => {
                        let _ = host
                            .workflow_fail_if_running(
                                run_id.clone(),
                                "Workflow coordinator ended before calling tyde_workflow_finish",
                            )
                            .await;
                        return;
                    }
                    None => {
                        let _ = host
                            .workflow_fail_if_running(
                                run_id.clone(),
                                "Workflow coordinator closed before calling tyde_workflow_finish",
                            )
                            .await;
                        return;
                    }
                    Some(_) => {}
                }
                if status_rx.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    async fn workflow_fail_if_running(
        &self,
        run_id: WorkflowRunId,
        message: &'static str,
    ) -> Result<(), String> {
        self.workflow_update_run_allow_terminal(run_id, |run| {
            if run.status != WorkflowRunSnapshotStatus::Running {
                return;
            }
            let now = crate::agent::now_ms();
            run.status = WorkflowRunSnapshotStatus::Failed;
            run.error = Some(message.to_owned());
            run.updated_at_ms = now;
            run.completed_at_ms = Some(now);
        })
        .await
        .map(|_| ())
    }

    fn schedule_agent_session_registration(
        &self,
        agent_id: AgentId,
        startup_rx: tokio::sync::oneshot::Receiver<Result<SessionId, String>>,
        visibility: SpawnVisibility,
    ) -> PendingAgentSessionPublication {
        let (publish_tx, mut publish_rx) = mpsc::unbounded_channel::<()>();
        let host = self.clone();
        let task_agent_id = agent_id.clone();
        tokio::spawn(async move {
            let agent_id = task_agent_id;
            let mut startup_rx = startup_rx;
            let startup_result = tokio::select! {
                startup = &mut startup_rx => Some(startup),
                publication = publish_rx.recv() => match publication {
                    Some(()) => None,
                    None => {
                        host.cleanup_unpublished_agent_session(agent_id.clone(), None, &visibility)
                            .await;
                        return;
                    }
                },
            };
            let (startup_result, publication_authorized) = match startup_result {
                Some(startup_result) => (startup_result, false),
                None => (startup_rx.await, true),
            };
            let session_id = match startup_result {
                Ok(Ok(session_id)) => {
                    let pending_inserted = {
                        let mut state = host.state.lock().await;
                        if let Some(existing_session_id) =
                            state.pending_agent_sessions.get(&agent_id)
                        {
                            Err(format!(
                                "agent {agent_id} already has pending session binding {existing_session_id} before startup registration"
                            ))
                        } else {
                            state
                                .pending_agent_sessions
                                .insert(agent_id.clone(), session_id.clone());
                            Ok(())
                        }
                    };
                    if let Err(error) = pending_inserted {
                        tracing::error!(
                            agent_id = %agent_id,
                            session_id = %session_id,
                            error = %error,
                            "failed to register pending agent session"
                        );
                        host.cleanup_unpublished_agent_session(agent_id.clone(), None, &visibility)
                            .await;
                        return;
                    }
                    session_id
                }
                Ok(Err(err)) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        error = %err,
                        "agent startup failed before session registration"
                    );
                    #[cfg(test)]
                    wait_for_startup_failure_fanout_race_test_hook(&host).await;
                    let failure_visibility = visibility.claim_startup_failure();
                    #[cfg(test)]
                    notify_startup_failure_claimed_test_hook(&host);
                    if failure_visibility == StartupFailureVisibility::AwaitPublication
                        && (publication_authorized || publish_rx.recv().await.is_some())
                    {
                        tracing::debug!(
                            agent_id = %agent_id,
                            error = %err,
                            "retaining published terminal startup failure for agent-stream observability"
                        );
                        return;
                    }
                    host.cleanup_unpublished_agent_session(agent_id.clone(), None, &visibility)
                        .await;
                    return;
                }
                Err(_) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        "agent startup channel dropped before session registration"
                    );
                    host.cleanup_unpublished_agent_session(agent_id.clone(), None, &visibility)
                        .await;
                    return;
                }
            };
            if !publication_authorized && publish_rx.recv().await.is_none() {
                host.cleanup_unpublished_agent_session(
                    agent_id.clone(),
                    Some(&session_id),
                    &visibility,
                )
                .await;
                return;
            }
            if visibility.cleanup_is_requested() {
                host.cleanup_unpublished_agent_session(
                    agent_id.clone(),
                    Some(&session_id),
                    &visibility,
                )
                .await;
                return;
            }
            if let Err(error) = host
                .publish_pending_agent_session(agent_id.clone(), session_id.clone())
                .await
            {
                tracing::error!(
                    agent_id = %agent_id,
                    session_id = %session_id,
                    error = %error,
                    "failed to publish pending agent session registration"
                );
                host.cleanup_unpublished_agent_session(agent_id, Some(&session_id), &visibility)
                    .await;
            }
        });
        PendingAgentSessionPublication {
            agent_id,
            publish_tx: Some(publish_tx),
        }
    }

    async fn publish_pending_agent_session(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let Some(pending_session_id) = state.pending_agent_sessions.get(&agent_id) else {
            return Err(format!(
                "missing pending session binding for agent {agent_id} during publication"
            ));
        };
        if pending_session_id != &session_id {
            return Err(format!(
                "pending session binding mismatch for agent {agent_id}: expected {session_id}, found {pending_session_id}"
            ));
        }
        if let Some(existing_session_id) = state.agent_sessions.get(&agent_id) {
            return Err(format!(
                "agent {agent_id} already has public session binding {existing_session_id} before pending publication"
            ));
        }

        state.pending_agent_sessions.remove(&agent_id);
        state
            .agent_sessions
            .insert(agent_id.clone(), session_id.clone());
        if let Some(store) = state.agents_view_preferences_store.clone()
            && let Err(error) = store.lock().await.promote_transient_agent(
                HostFilterId(LOCAL_HOST_ID.to_owned()),
                agent_id.clone(),
                session_id.clone(),
            )
        {
            state.agent_sessions.remove(&agent_id);
            state
                .pending_agent_sessions
                .insert(agent_id.clone(), session_id.clone());
            return Err(format!(
                "failed to promote agent annotations for agent {agent_id} session {session_id}: {error}"
            ));
        }
        fan_out_current_agents_view_preferences(&mut state).await;
        drop(state);
        self.fan_out_session_lists().await;
        self.deliver_submitted_reviews_for_session(session_id).await;
        Ok(())
    }

    async fn cleanup_unpublished_agent_session(
        &self,
        agent_id: AgentId,
        expected_session_id: Option<&SessionId>,
        visibility: &SpawnVisibility,
    ) {
        let removed_pending_session_id = {
            let mut state = self.state.lock().await;
            let should_remove = expected_session_id.is_some_and(|expected| {
                state.pending_agent_sessions.get(&agent_id) == Some(expected)
            });
            should_remove
                .then(|| state.pending_agent_sessions.remove(&agent_id))
                .flatten()
        };
        let removed_pending = removed_pending_session_id.is_some();
        if let Some(session_id) = &removed_pending_session_id {
            tracing::debug!(
                agent_id = %agent_id,
                session_id = %session_id,
                "cleaned pending agent session registration"
            );
        }
        visibility.request_cleanup().await;
        tracing::debug!(
            agent_id = %agent_id,
            "unpublished agent visibility quiesced before actor cleanup"
        );
        #[cfg(test)]
        wait_for_spawn_cancelled_before_cleanup_test_hook(self).await;
        let closed = self.close_agent_for_recorded_visibility(&agent_id).await;
        tracing::debug!(
            agent_id = %agent_id,
            closed,
            "unpublished agent actor cleanup completed"
        );
        if removed_pending {
            let mut state = self.state.lock().await;
            if let Some(store) = state.agents_view_preferences_store.clone()
                && let Err(error) = store.lock().await.remove_transient_agent(
                    HostFilterId(LOCAL_HOST_ID.to_owned()),
                    agent_id.clone(),
                )
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %error,
                    "failed to remove unpublished agent annotations during cleanup"
                );
            }
            fan_out_current_agents_view_preferences(&mut state).await;
        }
    }

    async fn wait_for_parent_session_id(
        &self,
        parent_agent_id: &AgentId,
    ) -> Result<SessionId, String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        tracing::debug!(
            parent_agent_id = %parent_agent_id,
            "waiting for backend-native child parent session registration"
        );
        loop {
            let session_id = {
                let state = self.state.lock().await;
                state
                    .agent_sessions
                    .get(parent_agent_id)
                    .cloned()
                    .or_else(|| state.pending_agent_sessions.get(parent_agent_id).cloned())
            };
            if let Some(session_id) = session_id {
                tracing::debug!(
                    parent_agent_id = %parent_agent_id,
                    parent_session_id = %session_id,
                    "resolved backend-native child parent session registration"
                );
                return Ok(session_id);
            }

            if tokio::time::Instant::now() >= deadline {
                let message = format!(
                    "cannot resolve parent session for backend-native child {}",
                    parent_agent_id
                );
                tracing::warn!(parent_agent_id = %parent_agent_id, "{message}");
                return Err(message);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn spawn_backend_native_subagent(
        &self,
        request: &HostSubAgentSpawnRequest,
    ) -> Result<SubAgentHandle, String> {
        tracing::info!(
            parent_agent_id = %request.parent_agent_id,
            tool_use_id = %request.tool_use_id,
            requested_name = %request.name,
            description = %request.description,
            agent_type = %request.agent_type,
            "host backend-native sub-agent spawn requested"
        );

        let (session_store, parent_handle) = {
            let state = self.state.lock().await;
            let parent_handle = state
                .registry
                .agent_handle(&request.parent_agent_id)
                .ok_or_else(|| {
                    format!(
                        "cannot resolve parent agent {} for backend-native child",
                        request.parent_agent_id
                    )
                })?;
            (Arc::clone(&state.session_store), parent_handle)
        };
        let parent_session_id = self
            .wait_for_parent_session_id(&request.parent_agent_id)
            .await?;

        let parent_start = parent_handle.snapshot();
        if parent_start.workspace_roots != request.workspace_roots {
            return Err(format!(
                "backend-native child workspace roots do not match parent {}",
                request.parent_agent_id
            ));
        }

        let session_id = request
            .session_id_hint
            .clone()
            .unwrap_or_else(|| SessionId(Uuid::new_v4().to_string()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (model_usage_tx, model_usage_rx) = mpsc::unbounded_channel();
        let (total_usage_tx, total_usage_rx) = mpsc::unbounded_channel();
        let (name_update_tx, mut name_update_rx) = mpsc::unbounded_channel::<String>();
        if !request.description.trim().is_empty() {
            event_tx
                .send(ChatEvent::MessageAdded(ChatMessage {
                    message_id: None,
                    timestamp: crate::agent::now_ms(),
                    sender: MessageSender::User,
                    content: request.description.clone(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    model_info: None,
                    token_usage: None,
                    context_breakdown: None,
                    images: None,
                }))
                .map_err(|_| "backend-native child prompt channel closed".to_owned())?;
        }
        let relay_request = RelaySpawnRequest {
            name: request.name.clone(),
            origin: AgentOrigin::BackendNative,
            custom_agent_id: parent_start.custom_agent_id.clone(),
            workflow: None,
            parent_agent_id: parent_start.agent_id.clone(),
            project_id: parent_start.project_id.clone(),
            backend_kind: parent_start.backend_kind,
            workspace_roots: parent_start.workspace_roots.clone(),
            session_id: session_id.clone(),
        };

        let (start, agent_handle, agent_visibility) = {
            let mut state = self.state.lock().await;
            let spawned = state.registry.spawn_relay(
                relay_request,
                event_rx,
                model_usage_rx,
                total_usage_rx,
                Arc::clone(&session_store),
            );
            state
                .agent_sessions
                .insert(spawned.start.agent_id.clone(), session_id.clone());
            (
                spawned.start,
                spawned.handle,
                state.agent_visibility.clone(),
            )
        };

        let host_streams = {
            let mut state = self.state.lock().await;
            let activity_summary =
                initial_agent_activity_summary_state(&mut state, &start.agent_id);
            state
                .host_streams
                .iter_mut()
                .filter_map(|(path, subscriber)| {
                    prepare_new_agent_fanout_for_subscriber(
                        subscriber,
                        &start,
                        &agent_handle,
                        activity_summary.clone(),
                    )
                    .map(
                        |(stream, attach_eagerly, instance_stream, activity_summary)| {
                            (
                                path.clone(),
                                stream,
                                attach_eagerly,
                                instance_stream,
                                activity_summary,
                            )
                        },
                    )
                })
                .collect::<Vec<_>>()
        };

        if let Err(err) = session_store.lock().await.upsert_backend_session(
            &BackendSession {
                id: session_id.clone(),
                backend_kind: start.backend_kind,
                workspace_roots: start.workspace_roots.clone(),
                title: Some(start.name.clone()),
                token_count: None,
                created_at_ms: Some(start.created_at_ms),
                updated_at_ms: Some(start.created_at_ms),
                resumable: false,
            },
            Some(parent_session_id),
            start.project_id.clone(),
            start.custom_agent_id.clone(),
            start.launch_profile_id.clone(),
        ) {
            let message = format!(
                "failed to persist backend-native child session {}: {err}",
                session_id
            );
            tracing::error!(
                parent_agent_id = %request.parent_agent_id,
                child_session_id = %session_id,
                "{message}"
            );
            let _ = self
                .close_agent_for_recorded_visibility(&start.agent_id)
                .await;
            return Err(message);
        }

        let mut dead_paths = Vec::new();
        let mut deferred_attachments = Vec::new();
        for (path, stream, attach_eagerly, instance_stream, activity_summary) in host_streams {
            match emit_new_agent_for_stream(
                &start,
                &agent_handle,
                &stream,
                instance_stream,
                attach_eagerly,
                activity_summary,
            ) {
                Ok(attachment) => {
                    agent_visibility.record_new_agent(start.agent_id.clone(), path.clone());
                    if let Some(attachment) = attachment {
                        deferred_attachments.push(attachment);
                    }
                }
                Err(_) => dead_paths.push(path),
            }
        }
        for attachment in deferred_attachments {
            self.attach_deferred_agent_stream(attachment).await;
        }
        if !dead_paths.is_empty() {
            let mut state = self.state.lock().await;
            for path in dead_paths {
                state.host_streams.remove(&path);
                state.agent_visibility.remove_host_stream(&path);
            }
        }
        {
            let mut state = self.state.lock().await;
            fan_out_current_agents_view_preferences(&mut state).await;
        }

        self.fan_out_session_lists().await;

        let naming_handle = agent_handle.clone();
        tokio::spawn(async move {
            while let Some(name) = name_update_rx.recv().await {
                if naming_handle.apply_generated_name(Ok(name)).await.is_none() {
                    break;
                }
            }
        });

        Ok(SubAgentHandle {
            event_tx,
            model_usage_tx,
            total_usage_tx,
            agent_id: start.agent_id,
            name_update_tx: Some(name_update_tx),
        })
    }

    pub(crate) async fn open_browse_stream(
        &self,
        connection_host_stream: &StreamPath,
        host_output_stream: &Stream,
        payload: HostBrowseStartPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "host_browse_start";
        let browse_stream_path = payload.browse_stream;
        if !browse_stream_path.0.starts_with("/browse/") {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "browse stream must start with /browse/, got {}",
                    browse_stream_path
                ),
            ));
        }
        let home = browse_stream::home_dir();
        let initial = self.resolve_browse_initial(payload.initial, &home).await;
        let browse_stream = host_output_stream.with_path(browse_stream_path.clone());

        {
            let mut state = self.state.lock().await;
            let previous = state.browse_streams.insert(
                (connection_host_stream.clone(), browse_stream_path.clone()),
                browse_stream.clone(),
            );
            if previous.is_some() {
                return Err(AppError::conflict(
                    OPERATION,
                    format!(
                        "duplicate browse stream registration for {}",
                        browse_stream_path
                    ),
                ));
            }
        }

        let opened = browse_stream::opened_payload(&home);
        let listing = match browse_stream::list_dir(&initial, payload.include_hidden).await {
            Ok(entries) => BrowseBootstrapListing::Entries { entries },
            Err(error) => BrowseBootstrapListing::Error { error },
        };
        let payload =
            serde_json::to_value(BrowseBootstrapPayload { opened, listing }).map_err(|error| {
                AppError::internal_message(
                    OPERATION,
                    "failed to serialize BrowseBootstrap payload",
                    error,
                )
            })?;
        let _ = browse_stream.send_value(FrameKind::BrowseBootstrap, payload);
        Ok(())
    }

    /// Resolve the client's browse-start intent to a concrete directory.
    /// `ProjectRoots` falls back to home when the project is unknown or its
    /// roots have no useful common ancestor — the browser should still open.
    async fn resolve_browse_initial(
        &self,
        initial: HostBrowseInitial,
        home: &HostAbsPath,
    ) -> HostAbsPath {
        match initial {
            HostBrowseInitial::Home => home.clone(),
            HostBrowseInitial::Path { path } => path,
            HostBrowseInitial::ProjectRoots { project_id } => {
                let project = {
                    let state = self.state.lock().await;
                    let project_store = state.project_store.lock().await;
                    project_store.get(&project_id)
                };
                project
                    .and_then(|project| {
                        browse_stream::project_roots_initial_path(&project.root_paths())
                    })
                    .unwrap_or_else(|| home.clone())
            }
        }
    }

    pub(crate) async fn list_browse_dir(
        &self,
        connection_host_stream: &StreamPath,
        browse_stream_path: &StreamPath,
        payload: HostBrowseListPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "host_browse_list";
        let browse_stream = {
            let state = self.state.lock().await;
            state
                .browse_streams
                .get(&(connection_host_stream.clone(), browse_stream_path.clone()))
                .cloned()
                .ok_or_else(|| {
                    AppError::not_found(
                        OPERATION,
                        format!(
                            "browse stream {} is not owned by host stream {}",
                            browse_stream_path, connection_host_stream
                        ),
                    )
                })?
        };

        match browse_stream::list_dir(&payload.path, payload.include_hidden).await {
            Ok(entries) => browse_stream::emit_entries(&browse_stream, &entries).await,
            Err(error) => browse_stream::emit_error(&browse_stream, &error).await,
        }
        Ok(())
    }

    pub(crate) async fn close_browse_stream(
        &self,
        connection_host_stream: &StreamPath,
        browse_stream_path: &StreamPath,
    ) {
        let mut state = self.state.lock().await;
        state
            .browse_streams
            .remove(&(connection_host_stream.clone(), browse_stream_path.clone()));
    }

    async fn ensure_host_project_subscription(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        operation: &'static str,
    ) -> AppResult<ProjectStreamHandle> {
        let mut state = self.state.lock().await;
        let summaries = state
            .review_registry
            .summaries(project_id.clone())
            .await
            .map_err(|error| project_command_error(operation, error))?;
        let handle = ensure_project_actor(&mut state, project_id)
            .await
            .map_err(|error| project_command_error(operation, error))?;
        handle
            .add_subscriber(
                connection_host_stream.clone(),
                project_output_stream.clone(),
                summaries,
            )
            .await
            .map_err(|error| project_command_error(operation, error))?;
        Ok(handle)
    }

    pub(crate) async fn read_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectReadFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_read_file";
        let handle = self
            .ensure_host_project_subscription(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                OPERATION,
            )
            .await?;
        // Route the read through the project-stream actor so the centralized
        // per-file version counter (the single bump point) assigns the version
        // carried on the contents the client renders.
        let contents = handle
            .read_file(payload)
            .await
            .map_err(|error| project_command_error(OPERATION, error))?;
        let payload = serde_json::to_value(&contents).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project file contents payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectFileContents, payload);
        Ok(())
    }

    /// Run a project-wide text search. The walk is offloaded to a blocking
    /// task that streams one `ProjectSearchResults` frame per matching file and
    /// a final `ProjectSearchComplete` frame, so the serial connection loop is
    /// never blocked. A newer search (or a matching cancel) for the same
    /// project supersedes any in-flight walk.
    pub(crate) async fn search_project_files(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectSearchPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_search";
        self.ensure_host_project_subscription(
            connection_host_stream,
            project_output_stream,
            project_id.clone(),
            OPERATION,
        )
        .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;

        // Register this search as the active one for the project and grab the
        // shared atomic the walk will poll for cancellation.
        let search_id = payload.search_id;
        let active = {
            let mut state = self.state.lock().await;
            let slot = state
                .project_search_ids
                .entry(project_id.clone())
                .or_insert_with(|| Arc::new(AtomicU64::new(0)));
            slot.store(search_id, Ordering::SeqCst);
            Arc::clone(slot)
        };

        let output = project_output_stream.clone();
        tokio::task::spawn_blocking(move || {
            let cancelled = {
                let active = Arc::clone(&active);
                move || active.load(Ordering::SeqCst) != search_id
            };
            let emit = {
                let active = Arc::clone(&active);
                let output = output.clone();
                move |file: ProjectSearchFileResult| -> bool {
                    // Re-check the active id immediately before sending: a long
                    // single-file scan may have outlived a cancel / supersede
                    // since the last per-file `cancelled` poll. Don't ship
                    // results the client will discard (or worse, mis-attribute).
                    if active.load(Ordering::SeqCst) != search_id {
                        return false;
                    }
                    match serde_json::to_value(&ProjectSearchResultsPayload { search_id, file }) {
                        Ok(value) => output
                            .send_value(FrameKind::ProjectSearchResults, value)
                            .is_ok(),
                        Err(_) => false,
                    }
                }
            };

            let (summary, error) = match search_project(&project, &payload, emit, cancelled) {
                Ok(summary) => (summary, None),
                Err(message) => (SearchSummary::default(), Some(message)),
            };

            // Suppress the terminal frame if this search was superseded — the
            // newer search owns the stream now.
            if active.load(Ordering::SeqCst) != search_id {
                return;
            }

            let complete = ProjectSearchCompletePayload {
                search_id,
                total_files: summary.total_files,
                total_matches: summary.total_matches,
                truncated: summary.truncated,
                cancelled: summary.cancelled,
                error,
            };
            if let Ok(value) = serde_json::to_value(&complete) {
                let _ = output.send_value(FrameKind::ProjectSearchComplete, value);
            }
        });

        Ok(())
    }

    /// Cancel an in-flight project search if `search_id` is still the active
    /// one for the project. Newer searches are left untouched.
    pub(crate) async fn cancel_project_search(
        &self,
        project_id: ProjectId,
        payload: ProjectSearchCancelPayload,
    ) -> AppResult<()> {
        let state = self.state.lock().await;
        if let Some(slot) = state.project_search_ids.get(&project_id) {
            // Only clear if it still points at the search being cancelled, so a
            // newer search that already replaced it keeps running.
            let _ = slot.compare_exchange(payload.search_id, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Subscribe a file for code intelligence. We resolve the file's current
    /// version from the project-stream actor's centralized counter so the
    /// pushed semantic model carries the version of the contents the client is
    /// rendering (the version-equals-rendered rule, spec §2.4), then delegate
    /// to the project's thin router → the owning root's `CodeIntelService`.
    /// Ensure the project subscription exists and **validate the path/root
    /// against the loaded project** (the same check the read path uses) before
    /// any code-intel routing. This is the security/robustness gate: a
    /// bad/unknown root fails here with a surfaced `CommandError` and never
    /// reaches the router, so a `CodeIntelService` is never spawned for a root
    /// that isn't a real project root. Returns the project-stream handle so the
    /// caller can reuse it (e.g. to peek the file version).
    async fn ensure_validated_code_intel_path(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        path: ProjectPath,
        operation: &'static str,
    ) -> AppResult<ProjectStreamHandle> {
        let handle = self
            .ensure_host_project_subscription(
                connection_host_stream,
                project_output_stream,
                project_id,
                operation,
            )
            .await?;
        handle
            .validate_path(path)
            .await
            .map_err(|error| project_command_error(operation, error))?;
        Ok(handle)
    }

    pub(crate) async fn project_accessed(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_accessed";
        self.ensure_host_project_subscription(
            connection_host_stream,
            project_output_stream,
            project_id.clone(),
            OPERATION,
        )
        .await?;
        self.warm_code_intel_project(project_id, OPERATION).await
    }

    async fn warm_code_intel_project(
        &self,
        project_id: ProjectId,
        operation: &'static str,
    ) -> AppResult<()> {
        let mut state = self.state.lock().await;
        let project = state
            .project_store
            .lock()
            .await
            .get(&project_id)
            .ok_or_else(|| {
                AppError::not_found(operation, format!("project {} not found", project_id))
            })?;
        let roots = project.root_paths();
        let handle = ensure_project_actor(&mut state, project_id.clone())
            .await
            .map_err(|error| project_command_error(operation, error))?;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(operation, anyhow!(error)))?
            .code_intel;
        let router = state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle, code_intel_settings));
        router.retain_roots(&roots);
        router.warm_project(roots);
        Ok(())
    }

    pub(crate) async fn code_intel_subscribe_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: CodeIntelSubscribeFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "code_intel_subscribe_file";
        let handle = self
            .ensure_validated_code_intel_path(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                payload.path.clone(),
                OPERATION,
            )
            .await?;
        let mut state = self.state.lock().await;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .code_intel;
        state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle.clone(), code_intel_settings))
            .subscribe(payload, project_output_stream.clone());
        Ok(())
    }

    pub(crate) async fn code_intel_unsubscribe_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: CodeIntelUnsubscribeFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "code_intel_unsubscribe_file";
        let handle = self
            .ensure_validated_code_intel_path(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                payload.path.clone(),
                OPERATION,
            )
            .await?;
        let mut state = self.state.lock().await;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .code_intel;
        state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle.clone(), code_intel_settings))
            .unsubscribe(payload.path);
        Ok(())
    }

    pub(crate) async fn code_intel_set_visible_range(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: CodeIntelSetVisibleRangePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "code_intel_set_visible_range";
        let handle = self
            .ensure_validated_code_intel_path(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                payload.path.clone(),
                OPERATION,
            )
            .await?;
        let mut state = self.state.lock().await;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .code_intel;
        state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle.clone(), code_intel_settings))
            .set_visible_range(payload);
        Ok(())
    }

    pub(crate) async fn code_intel_hover(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: CodeIntelHoverPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "code_intel_hover";
        let handle = self
            .ensure_validated_code_intel_path(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                payload.path.clone(),
                OPERATION,
            )
            .await?;
        let mut state = self.state.lock().await;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .code_intel;
        state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle.clone(), code_intel_settings))
            .hover(payload, project_output_stream.clone());
        Ok(())
    }

    pub(crate) async fn code_intel_navigate(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: CodeIntelNavigatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "code_intel_navigate";
        let handle = self
            .ensure_validated_code_intel_path(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                payload.path.clone(),
                OPERATION,
            )
            .await?;
        let mut state = self.state.lock().await;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .code_intel;
        state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle.clone(), code_intel_settings))
            .navigate(payload, project_output_stream.clone());
        Ok(())
    }

    pub(crate) async fn code_intel_find_references(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: CodeIntelFindReferencesPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "code_intel_find_references";
        let handle = self
            .ensure_validated_code_intel_path(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                payload.path.clone(),
                OPERATION,
            )
            .await?;
        let mut state = self.state.lock().await;
        let code_intel_settings = state
            .settings_store
            .lock()
            .await
            .get()
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?
            .code_intel;
        state
            .code_intel_routers
            .entry(project_id)
            .or_insert_with(|| CodeIntelRouter::new(handle.clone(), code_intel_settings))
            .find_references(payload, project_output_stream.clone());
        Ok(())
    }

    pub(crate) async fn code_intel_cancel_references(
        &self,
        project_id: ProjectId,
        payload: CodeIntelCancelReferencesPayload,
    ) -> AppResult<()> {
        // Cancel carries no path, so there is nothing to validate and nothing
        // to spawn — it only broadcasts to services that already exist.
        let mut state = self.state.lock().await;
        if let Some(router) = state.code_intel_routers.get_mut(&project_id) {
            router.cancel_references(payload);
        }
        Ok(())
    }

    pub(crate) async fn list_project_dir(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectListDirPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_list_dir";
        self.ensure_host_project_subscription(
            connection_host_stream,
            project_output_stream,
            project_id.clone(),
            OPERATION,
        )
        .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let listing = build_dir_listing(&project, &payload.root, &payload.path)
            .map_err(|error| project_command_error(OPERATION, error))?;
        let payload = serde_json::to_value(&listing).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project dir listing payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectFileList, payload);
        Ok(())
    }

    pub(crate) async fn read_project_diff(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ProjectReadDiffPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_read_diff";
        let handle = self
            .ensure_host_project_subscription(
                connection_host_stream,
                project_output_stream,
                project_id.clone(),
                OPERATION,
            )
            .await?;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let diff = read_diff(&project, payload)
            .map_err(|error| project_command_error(OPERATION, error))?;
        handle
            .remember_diff_context_mode(
                connection_host_stream.clone(),
                ProjectDiffRequestKey {
                    root: diff.root.clone(),
                    scope: diff.scope,
                    path: diff.path.clone(),
                },
                diff.context_mode,
            )
            .await
            .map_err(|error| project_command_error(OPERATION, error))?;
        let payload = serde_json::to_value(&diff).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project git diff payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectGitDiff, payload);
        Ok(())
    }

    pub(crate) async fn stage_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectStageFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_stage_file";
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        stage_file(&project, &path).map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn stage_project_hunk(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectStageHunkPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_stage_hunk";
        let path = payload.path;
        let hunk_id = payload.hunk_id;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        stage_hunk(&project, &path, &hunk_id)
            .map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn unstage_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectUnstageFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_unstage_file";
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        unstage_file(&project, &path).map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn discard_project_file(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectDiscardFilePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_discard_file";
        let path = payload.path;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        discard_file(&project, &path).map_err(|error| project_command_error(OPERATION, error))?;
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            Some(path),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn commit_project(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        payload: ProjectGitCommitPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "project_git_commit";
        let root = payload.root;
        let message = payload.message;
        let project_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let commit_hash = commit(&project, &root, &message)
            .map_err(|error| project_command_error(OPERATION, error))?;
        let result_payload = ProjectGitCommitResultPayload {
            root: root.clone(),
            commit_hash,
        };
        let result_payload = serde_json::to_value(&result_payload).map_err(|error| {
            AppError::internal_message(
                OPERATION,
                "failed to serialize project git commit result payload",
                error,
            )
        })?;
        let _ = project_output_stream.send_value(FrameKind::ProjectGitCommitResult, result_payload);
        self.refresh_after_project_mutation(
            connection_host_stream,
            project_output_stream.clone(),
            project_id,
            None,
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn create_review(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: &Stream,
        project_id: ProjectId,
        payload: ReviewCreatePayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "review_create";
        tracing::info!(
            project_id = %project_id,
            selection_kind = payload.selection.kind_name(),
            connection_stream = %connection_host_stream,
            output_stream = %project_output_stream.path(),
            "received review_create"
        );
        let (project_store, review_registry) = {
            let state = self.state.lock().await;
            (
                Arc::clone(&state.project_store),
                state.review_registry.clone(),
            )
        };

        let project = load_project(&project_store, &project_id, OPERATION).await?;
        let normalized_selection = review_create_selection(&project, &payload.selection)
            .map_err(|error| AppError::invalid(OPERATION, error))?;
        let selection_root = match &normalized_selection {
            ReviewDiffSelection::Root { root, .. } => Some(root),
            ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => None,
        };

        let diff_started = Instant::now();
        tracing::debug!(
            project_id = %project_id,
            selection_kind = payload.selection.kind_name(),
            root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
            "reading initial review diffs"
        );
        let diffs = match read_review_diffs(&project, &normalized_selection) {
            Ok(diffs) => {
                let stats = host_diff_stats(&diffs);
                tracing::info!(
                    project_id = %project_id,
                    selection_kind = payload.selection.kind_name(),
                    root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
                    diff_count = stats.diff_count,
                    file_count = stats.file_count,
                    hunk_count = stats.hunk_count,
                    line_count = stats.line_count,
                    elapsed_ms = diff_started.elapsed().as_millis() as u64,
                    "read initial review diffs"
                );
                diffs
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    selection_kind = payload.selection.kind_name(),
                    root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
                    elapsed_ms = diff_started.elapsed().as_millis() as u64,
                    error_len = error.len(),
                    "failed to read initial review diffs"
                );
                return Err(AppError::internal_message(
                    OPERATION,
                    error.clone(),
                    anyhow!(error),
                ));
            }
        };
        let review_id = ReviewId(Uuid::new_v4().to_string());
        tracing::debug!(
            project_id = %project_id,
            review_id = %review_id,
            root = selection_root.map(|root| root.0.as_str()).unwrap_or("<workspace>"),
            "creating or getting review actor"
        );
        let request = build_create_request(
            review_id.clone(),
            project_id.clone(),
            ReviewCreatePayload {
                selection: normalized_selection,
            },
            diffs,
        );
        let review_id = review_registry.create(request).await.map_err(|error| {
            AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
        })?;
        let review_stream = project_output_stream.with_path(review_stream_path(&review_id));
        review_registry
            .subscribe(
                review_id.clone(),
                connection_host_stream.clone(),
                review_stream,
                true,
            )
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        tracing::info!(review_id = %review_id, project_id = %project_id, "created or attached review");
        Ok(())
    }

    pub(crate) async fn review_action(
        &self,
        connection_host_stream: &StreamPath,
        review_output_stream: Stream,
        review_id: ReviewId,
        payload: ReviewActionPayload,
    ) -> AppResult<()> {
        const OPERATION: &str = "review_action";
        tracing::info!(
            review_id = %review_id,
            action_kind = payload.kind_name(),
            connection_stream = %connection_host_stream,
            output_stream = %review_output_stream.path(),
            "received review_action"
        );
        let review_registry = {
            let state = self.state.lock().await;
            state.review_registry.clone()
        };
        review_registry
            .action(
                review_id,
                payload,
                connection_host_stream.clone(),
                review_output_stream,
            )
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        Ok(())
    }

    pub(crate) async fn review_subscribe(
        &self,
        connection_host_stream: &StreamPath,
        review_output_stream: Stream,
        review_id: ReviewId,
        include_diffs: bool,
    ) -> AppResult<()> {
        const OPERATION: &str = "review_subscribe";
        tracing::info!(
            review_id = %review_id,
            connection_stream = %connection_host_stream,
            output_stream = %review_output_stream.path(),
            include_diffs,
            "received review_subscribe"
        );
        let review_registry = {
            let state = self.state.lock().await;
            state.review_registry.clone()
        };
        review_registry
            .subscribe(
                review_id,
                connection_host_stream.clone(),
                review_output_stream,
                include_diffs,
            )
            .await
            .map_err(|error| {
                AppError::internal_message(OPERATION, error.clone(), anyhow!(error))
            })?;
        Ok(())
    }

    async fn deliver_review_payload(
        &self,
        review_id: ReviewId,
        project_id: ProjectId,
        target: ReviewSubmitTarget,
        payload: SendMessagePayload,
    ) -> ReviewDeliveryOutcome {
        let message_len = payload.message.len();
        let images_count = payload.images.as_ref().map_or(0, Vec::len);
        tracing::debug!(
            review_id = %review_id,
            project_id = %project_id,
            target = ?target,
            message_len,
            images_count,
            "resolving review delivery"
        );
        let Some(protocol::MessageOrigin::Review {
            review_id: origin_review_id,
        }) = payload.origin.as_ref()
        else {
            tracing::warn!(
                review_id = %review_id,
                project_id = %project_id,
                reason = "missing_origin",
                "review delivery failed before target resolution"
            );
            return ReviewDeliveryOutcome::Failed(
                "review delivery payload did not carry review origin".to_owned(),
            );
        };
        if origin_review_id != &review_id {
            tracing::warn!(
                review_id = %review_id,
                origin_review_id = %origin_review_id,
                project_id = %project_id,
                reason = "origin_mismatch",
                "review delivery failed before target resolution"
            );
            return ReviewDeliveryOutcome::Failed(format!(
                "review delivery payload origin {} did not match {}",
                origin_review_id, review_id
            ));
        }

        match target {
            ReviewSubmitTarget::ExistingAgent { agent_id } => {
                let target_agent = {
                    let state = self.state.lock().await;
                    let Some(handle) = state.registry.agent_handle(&agent_id) else {
                        tracing::info!(
                            review_id = %review_id,
                            project_id = %project_id,
                            target_agent_id = %agent_id,
                            outcome = "offline",
                            "review delivery target is offline"
                        );
                        return ReviewDeliveryOutcome::Offline;
                    };
                    let start = handle.snapshot();
                    if start.project_id.as_ref() != Some(&project_id) {
                        tracing::warn!(
                            review_id = %review_id,
                            project_id = %project_id,
                            target_agent_id = %agent_id,
                            target_project_id = ?start.project_id,
                            "review delivery target is not in the review project"
                        );
                        return ReviewDeliveryOutcome::Failed(format!(
                            "agent {} is not bound to project {}",
                            agent_id, project_id
                        ));
                    }
                    handle
                };
                if target_agent
                    .send_input(protocol::AgentInput::SendMessage(payload))
                    .await
                {
                    tracing::info!(
                        review_id = %review_id,
                        project_id = %project_id,
                        target_agent_id = %agent_id,
                        message_len,
                        images_count,
                        "delivered review feedback bundle to live agent"
                    );
                    ReviewDeliveryOutcome::Delivered {
                        target_agent_id: agent_id,
                    }
                } else {
                    tracing::warn!(
                        review_id = %review_id,
                        project_id = %project_id,
                        target_agent_id = %agent_id,
                        outcome = "offline",
                        "review delivery target went offline"
                    );
                    ReviewDeliveryOutcome::Offline
                }
            }
            ReviewSubmitTarget::NewAgent {
                backend_kind,
                cost_hint,
                custom_agent_id,
                name,
                instructions,
            } => match self
                .spawn_review_target_agent(ReviewTargetAgentRequest {
                    review_id: review_id.clone(),
                    project_id: project_id.clone(),
                    backend_kind,
                    cost_hint,
                    custom_agent_id,
                    name,
                    instructions,
                    payload,
                })
                .await
            {
                Ok(agent_id) => ReviewDeliveryOutcome::Delivered {
                    target_agent_id: agent_id,
                },
                Err(message) => ReviewDeliveryOutcome::Failed(message),
            },
        }
    }

    async fn spawn_review_target_agent(
        &self,
        request: ReviewTargetAgentRequest,
    ) -> Result<AgentId, String> {
        let ReviewTargetAgentRequest {
            review_id,
            project_id,
            backend_kind,
            cost_hint,
            custom_agent_id,
            name,
            instructions,
            mut payload,
        } = request;
        let (
            project,
            settings_store,
            custom_agent_store,
            mcp_server_store,
            steering_store,
            skill_store,
            use_mock_backend,
            debug_mcp,
            agent_control_mcp,
            config_mcp,
        ) = {
            let state = self.state.lock().await;
            let project = state
                .project_store
                .lock()
                .await
                .get(&project_id)
                .ok_or_else(|| {
                    format!("cannot spawn review target in missing project {project_id}")
                })?;
            (
                project,
                Arc::clone(&state.settings_store),
                Arc::clone(&state.custom_agent_store),
                Arc::clone(&state.mcp_server_store),
                Arc::clone(&state.steering_store),
                Arc::clone(&state.skill_store),
                state.use_mock_backend,
                state.debug_mcp.clone(),
                state.agent_control_mcp.clone(),
                state.config_mcp.clone(),
            )
        };
        let project_roots = project
            .root_paths()
            .into_iter()
            .map(|root| root.0)
            .collect::<Vec<_>>();
        if project_roots.is_empty() {
            return Err(format!(
                "project {} has no roots for review target",
                project_id
            ));
        }

        let host_settings =
            settings_store.lock().await.get().map_err(|error| {
                format!("failed to load host settings for review target: {error}")
            })?;
        let startup_mcp_servers = startup_mcp_servers_for_settings(
            &host_settings,
            &project_roots,
            &debug_mcp,
            &agent_control_mcp,
            &config_mcp,
            None,
        );
        let mut resolved_spawn_config = {
            let custom_agents = custom_agent_store.lock().await;
            let mcp_servers = mcp_server_store.lock().await;
            let steering = steering_store.lock().await;
            let skills = skill_store.lock().await;
            resolve_spawn_config(ResolveSpawnConfigRequest {
                backend_kind,
                project_id: Some(&project_id),
                custom_agent_id: custom_agent_id.as_ref(),
                built_in_mcp_servers: &startup_mcp_servers,
                custom_agent_store: &custom_agents,
                mcp_server_store: &mcp_servers,
                steering_store: &steering,
                skill_store: &skills,
            })
            .map_err(|error| format!("failed to resolve review target agent config: {error}"))?
        };
        resolved_spawn_config.access_mode = protocol::BackendAccessMode::Unrestricted;
        let startup_mcp_servers =
            protocol_mcp_servers_to_startup(&resolved_spawn_config.mcp_servers);
        let session_settings_schema = {
            let state = self.state.lock().await;
            session_schema_for_backend(&state, backend_kind)
        };
        let session_settings_schema = if backend_has_dynamic_session_schema(backend_kind)
            && session_settings_schema.is_none()
        {
            self.refresh_session_schema_for_backend(backend_kind).await;
            let state = self.state.lock().await;
            session_schema_for_backend(&state, backend_kind)
        } else {
            session_settings_schema
        };
        if let Some(instructions) = instructions
            && !instructions.trim().is_empty()
        {
            payload.message = format!(
                "Instructions for this new review target agent:\n{}\n\n{}",
                instructions.trim(),
                payload.message
            );
        }
        let name = name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Review Feedback".to_owned());
        let request = ResolvedSpawnRequest {
            name: name.clone(),
            origin: protocol::AgentOrigin::AgentControl,
            custom_agent_id,
            team_id: None,
            team_member_id: None,
            workflow: None,
            parent_agent_id: None,
            parent_session_id: None,
            project_id: Some(project_id.clone()),
            backend_kind,
            launch_profile_id: None,
            workspace_roots: project_roots,
            initial_input: Some(payload),
            cost_hint,
            session_settings: None,
            session_settings_schema,
            backend_config: Default::default(),
            startup_mcp_servers,
            resolved_spawn_config,
            resume_session_id: None,
            fork_from_session_id: None,
            startup_warning: None,
            startup_failure: None,
            initial_alias: Some(InitialAgentAlias {
                name,
                persistence: InitialAgentAliasPersistence::User,
            }),
            use_mock_backend,
        };
        let agent_id = self.spawn_resolved_agent(request).await;
        tracing::info!(
            review_id = %review_id,
            project_id = %project_id,
            target_agent_id = %agent_id,
            backend_kind = ?backend_kind,
            "spawned review target agent"
        );
        Ok(agent_id)
    }

    async fn deliver_submitted_reviews_for_session(&self, session_id: SessionId) {
        tracing::debug!(
            session_id = %session_id,
            "legacy submitted review redelivery is disabled for inline reviews"
        );
    }

    async fn emit_review_list_changed(&self, project_id: ProjectId) {
        let (registry, handle) = {
            let mut state = self.state.lock().await;
            let registry = state.review_registry.clone();
            let handle = match ensure_project_actor(&mut state, project_id.clone()).await {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(
                        project_id = %project_id,
                        error = %error,
                        "failed to ensure project actor for review list update"
                    );
                    return;
                }
            };
            (registry, handle)
        };
        let summaries = match registry.summaries(project_id.clone()).await {
            Ok(summaries) => summaries,
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "failed to build review summaries"
                );
                return;
            }
        };
        if let Err(error) = handle
            .emit_project_event(protocol::ProjectEventPayload::ReviewListChanged {
                reviews: summaries,
            })
            .await
        {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to emit review list update"
            );
        }
    }

    async fn spawn_ai_reviewer(
        &self,
        request: ReviewAiSpawnRequest,
    ) -> (
        oneshot::Sender<Result<AgentId, String>>,
        Result<AgentId, String>,
    ) {
        let reply = request.reply;
        let requested_backend_kind = request.backend_kind;
        let backend_kind = match self
            .resolve_ai_reviewer_backend_kind(requested_backend_kind)
            .await
        {
            Ok(backend_kind) => backend_kind,
            Err(message) => return (reply, Err(message)),
        };
        let instructions_len = request.instructions.as_ref().map_or(0, String::len);
        let roots = {
            let state = self.state.lock().await;
            let project = match state
                .project_store
                .lock()
                .await
                .get(&request.review.project_id)
            {
                Some(project) => project,
                None => {
                    return (
                        reply,
                        Err(format!(
                            "cannot spawn AI reviewer for missing project {}",
                            request.review.project_id
                        )),
                    );
                }
            };
            project
                .root_paths()
                .into_iter()
                .map(|root| root.0)
                .collect::<Vec<_>>()
        };
        let roots_count = roots.len();
        let stats = host_diff_stats(&request.review.diffs);
        tracing::info!(
            review_id = %request.review_id,
            backend_kind = ?backend_kind,
            requested_backend_kind = ?requested_backend_kind,
            roots_count,
            diff_count = stats.diff_count,
            file_count = stats.file_count,
            hunk_count = stats.hunk_count,
            line_count = stats.line_count,
            instructions_len,
            "spawning AI reviewer"
        );
        if roots_count == 0 {
            tracing::warn!(
                review_id = %request.review_id,
                backend_kind = ?backend_kind,
                "AI reviewer spawn rejected without diff roots"
            );
            return (reply, Err("review has no frozen diff roots".to_owned()));
        }
        if stats.file_count == 0 {
            tracing::warn!(
                review_id = %request.review_id,
                backend_kind = ?backend_kind,
                roots_count,
                "AI reviewer spawn rejected without changed files"
            );
            return (
                reply,
                Err("review has no changed files to review".to_owned()),
            );
        }
        let review_mcp_url = {
            let state = self.state.lock().await;
            state.review_mcp.url.clone()
        };
        if review_mcp_url.trim().is_empty() {
            tracing::warn!(
                review_id = %request.review_id,
                backend_kind = ?backend_kind,
                "AI reviewer spawn rejected without review MCP URL"
            );
            return (
                reply,
                Err("review feedback MCP server is unavailable for AI review".to_owned()),
            );
        }
        let reviewer_system_prompt =
            build_reviewer_system_prompt(&request.review, request.instructions);
        let reviewer_system_prompt_len = reviewer_system_prompt.len();
        let reviewer_spawn_config = ResolvedSpawnConfig {
            instructions: Some(reviewer_system_prompt),
            steering_body: String::new(),
            skills: Vec::new(),
            mcp_servers: vec![McpServerConfig {
                id: McpServerId("tyde-review-feedback".to_owned()),
                name: REVIEW_FEEDBACK_MCP_SERVER_NAME.to_owned(),
                transport: McpTransportConfig::Http {
                    url: review_mcp_url,
                    headers: HashMap::new(),
                    bearer_token_env_var: None,
                },
            }],
            tool_policy: reviewer_tool_policy(),
            access_mode: protocol::BackendAccessMode::ReadOnly,
        };
        let prompt = build_reviewer_user_prompt();
        let prompt_len = prompt.len();
        let payload = SpawnAgentPayload {
            name: Some("AI Review".to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(request.review.project_id.clone()),
            params: SpawnAgentParams::New {
                workspace_roots: roots,
                prompt,
                images: None,
                backend_kind,
                launch_profile_id: None,
                cost_hint: request.cost_hint,
                access_mode: protocol::BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        };
        tracing::debug!(
            review_id = %request.review_id,
            backend_kind = ?backend_kind,
            roots_count,
            reviewer_system_prompt_len,
            prompt_len,
            "dispatching AI reviewer spawn"
        );
        let agent_id = self
            .spawn_agent_with_origin_and_config(
                payload,
                protocol::AgentOrigin::AgentControl,
                Some(reviewer_spawn_config),
                None,
            )
            .await
            .map_err(|error| error.to_string());
        let agent_id = match agent_id {
            Ok(agent_id) => agent_id,
            Err(error) => return (reply, Err(error)),
        };
        if let Some(agent_handle) = self.agent_handle(&agent_id).await {
            tracing::info!(
                review_id = %request.review_id,
                reviewer_agent_id = %agent_id,
                "AI reviewer spawned; attaching tool bridge"
            );
            ReviewerToolBridge::spawn(agent_id.clone(), agent_handle, request.review_handle);
            (reply, Ok(agent_id))
        } else {
            tracing::warn!(
                review_id = %request.review_id,
                reviewer_agent_id = %agent_id,
                "AI reviewer spawned but tool bridge attach target was missing"
            );
            (
                reply,
                Err(format!(
                    "spawned AI reviewer {} but could not attach tool bridge",
                    agent_id
                )),
            )
        }
    }

    async fn resolve_ai_reviewer_backend_kind(
        &self,
        requested: Option<protocol::BackendKind>,
    ) -> Result<protocol::BackendKind, String> {
        if let Some(backend_kind) = requested {
            return Ok(backend_kind);
        }
        let settings_store = {
            let state = self.state.lock().await;
            Arc::clone(&state.settings_store)
        };
        let settings = settings_store
            .lock()
            .await
            .get()
            .map_err(|error| format!("failed to load host settings for AI review: {error}"))?;
        settings
            .default_backend
            .or_else(|| settings.enabled_backends.first().copied())
            .ok_or_else(|| {
                "backend_kind is required because the host has no default_backend or enabled backends"
                    .to_owned()
            })
    }
}

async fn load_project(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    operation: &'static str,
) -> AppResult<Project> {
    let projects = project_store
        .lock()
        .await
        .list()
        .map_err(|error| project_store_error(operation, error))?;
    projects
        .into_iter()
        .find(|project| &project.id == project_id)
        .ok_or_else(|| AppError::not_found(operation, format!("project {} not found", project_id)))
}

fn read_review_diffs(
    project: &Project,
    selection: &ReviewDiffSelection,
) -> Result<Vec<protocol::ProjectGitDiffPayload>, String> {
    match selection {
        ReviewDiffSelection::AllUncommitted | ReviewDiffSelection::Workspace { .. } => {
            let mut diffs = Vec::new();
            for root in project.root_paths() {
                let payload = ProjectReadDiffPayload {
                    root,
                    scope: protocol::ProjectDiffScope::Unstaged,
                    path: None,
                    context_mode: protocol::DiffContextMode::FullFile,
                };
                match read_diff(project, payload) {
                    Ok(diff) => diffs.push(diff),
                    Err(error) if is_not_git_repository_error(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(diffs)
        }
        ReviewDiffSelection::Root { root, path, .. } => {
            let payload = ProjectReadDiffPayload {
                root: root.clone(),
                scope: protocol::ProjectDiffScope::Unstaged,
                path: path.clone(),
                context_mode: protocol::DiffContextMode::FullFile,
            };
            match read_diff(project, payload) {
                Ok(diff) => Ok(vec![diff]),
                Err(error) if is_not_git_repository_error(&error) => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct HostDiffStats {
    diff_count: usize,
    file_count: usize,
    hunk_count: usize,
    line_count: usize,
}

fn host_diff_stats(diffs: &[protocol::ProjectGitDiffPayload]) -> HostDiffStats {
    let file_count = diffs.iter().map(|diff| diff.files.len()).sum();
    let hunk_count = diffs
        .iter()
        .flat_map(|diff| diff.files.iter())
        .map(|file| file.hunks.len())
        .sum();
    let line_count = diffs
        .iter()
        .flat_map(|diff| diff.files.iter())
        .flat_map(|file| file.hunks.iter())
        .map(|hunk| hunk.lines.len())
        .sum();
    HostDiffStats {
        diff_count: diffs.len(),
        file_count,
        hunk_count,
        line_count,
    }
}

fn compute_workbench_roots(
    parent_roots: &[ProjectRootPath],
    branch: &GitBranchName,
) -> AppResult<Vec<WorkbenchRoot>> {
    parent_roots
        .iter()
        .map(|parent_root| {
            Ok(WorkbenchRoot {
                parent_root: parent_root.clone(),
                worktree_root: compute_worktree_path(parent_root, branch)
                    .map_err(|error| AppError::invalid("workbench_create", error))?,
            })
        })
        .collect()
}

fn compute_worktree_path(
    parent_root: &ProjectRootPath,
    branch: &GitBranchName,
) -> Result<ProjectRootPath, String> {
    let parent_path = Path::new(&parent_root.0);
    let basename = parent_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("parent root {} has no UTF-8 basename", parent_root))?;
    let sibling_name = format!("{}--{}", basename, sanitize_branch_for_path(&branch.0));
    let worktree_path = match parent_path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => PathBuf::from(sibling_name),
        Some(parent) => parent.join(sibling_name),
        None => PathBuf::from(sibling_name),
    };
    let worktree_path = worktree_path.to_str().ok_or_else(|| {
        format!(
            "computed worktree path for parent root {} is not UTF-8",
            parent_root
        )
    })?;
    Ok(ProjectRootPath(worktree_path.to_owned()))
}

/// Maps a branch name to a directory-name-safe form. Percent-encoding is
/// deliberately avoided: LLVM's output-file creation treats `%` in a path as
/// a unique-name placeholder, so rust-lld cannot link inside a directory
/// whose name contains `%` (wasm builds fail with "cannot open output file").
fn sanitize_branch_for_path(branch: &str) -> String {
    let mut sanitized = String::with_capacity(branch.len());
    for character in branch.chars() {
        match character {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => {
                sanitized.push(character);
            }
            _ => sanitized.push('-'),
        }
    }
    sanitized
}

async fn preflight_workbench_create(
    project_store: &Arc<Mutex<ProjectStore>>,
    parent: &Project,
    branch: &GitBranchName,
    roots: &[WorkbenchRoot],
) -> AppResult<()> {
    const OPERATION: &str = "workbench_create";
    if !matches!(parent.source, ProjectSource::Standalone { .. }) {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "cannot create workbench for non-standalone parent project {}",
                parent.id
            ),
        ));
    }

    for root in roots {
        ensure_git_top_level(&root.parent_root).await?;
    }
    for root in roots {
        let output = run_git(
            &root.parent_root,
            &["check-ref-format", "--branch", &branch.0],
        )
        .await
        .map_err(|error| AppError::internal_message(OPERATION, error.clone(), anyhow!(error)))?;
        if !output.status.success() {
            return Err(AppError::invalid(
                OPERATION,
                format!(
                    "invalid git branch {} for parent root {}: {}",
                    branch,
                    root.parent_root,
                    git_output_message(&output)
                ),
            ));
        }
    }
    for root in roots {
        if git_branch_exists(&root.parent_root, branch).await? {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "branch {} already exists in parent root {}",
                    branch, root.parent_root
                ),
            ));
        }
    }
    for root in roots {
        let exists = tokio::fs::try_exists(&root.worktree_root.0)
            .await
            .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
        if exists {
            return Err(AppError::conflict(
                OPERATION,
                format!("worktree path {} already exists", root.worktree_root),
            ));
        }
    }

    let records = project_store
        .lock()
        .await
        .list()
        .map_err(|error| project_store_error(OPERATION, error))?;
    for root in roots {
        if let Some(owner) = records.iter().find(|project| {
            project
                .root_paths()
                .into_iter()
                .any(|candidate| candidate == root.worktree_root)
        }) {
            return Err(AppError::conflict(
                OPERATION,
                format!(
                    "worktree path {} is already registered as project root for project {}",
                    root.worktree_root, owner.id
                ),
            ));
        }
    }

    Ok(())
}

async fn ensure_git_top_level(parent_root: &ProjectRootPath) -> AppResult<()> {
    const OPERATION: &str = "workbench_create";
    let output = run_git(parent_root, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|error| AppError::internal_message(OPERATION, error.clone(), anyhow!(error)))?;
    if !output.status.success() {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "parent root {} is not a git top-level: {}",
                parent_root,
                git_output_message(&output)
            ),
        ));
    }
    let top_level = String::from_utf8(output.stdout).map_err(|error| {
        AppError::internal_message(
            OPERATION,
            format!(
                "git rev-parse --show-toplevel for {} returned non-UTF-8 output",
                parent_root
            ),
            error,
        )
    })?;
    let top_level = top_level.trim();
    let root_canonical = tokio::fs::canonicalize(&parent_root.0)
        .await
        .map_err(|error| {
            AppError::invalid(
                OPERATION,
                format!("parent root {} is invalid: {}", parent_root, error),
            )
        })?;
    let top_level_canonical = tokio::fs::canonicalize(top_level)
        .await
        .map_err(|error| AppError::internal(OPERATION, anyhow!(error)))?;
    if root_canonical != top_level_canonical {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "parent root {} is not a git top-level; git top-level is {}",
                parent_root, top_level
            ),
        ));
    }
    Ok(())
}

async fn git_branch_exists(
    parent_root: &ProjectRootPath,
    branch: &GitBranchName,
) -> AppResult<bool> {
    let ref_name = format!("refs/heads/{}", branch.0);
    let output = run_git(
        parent_root,
        &["rev-parse", "--verify", "--quiet", &ref_name],
    )
    .await
    .map_err(|error| {
        AppError::internal_message("workbench_create", error.clone(), anyhow!(error))
    })?;
    Ok(output.status.success())
}

async fn git_worktree_add(
    parent_root: &ProjectRootPath,
    worktree_root: &ProjectRootPath,
    branch: &GitBranchName,
    base_commit: &str,
) -> Result<(), String> {
    let output = run_git(
        parent_root,
        &[
            "worktree",
            "add",
            "-b",
            &branch.0,
            &worktree_root.0,
            base_commit,
        ],
    )
    .await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree add failed for parent root {} branch {} worktree {}: {}",
        parent_root,
        branch,
        worktree_root,
        git_output_message(&output)
    ))
}

async fn git_resolve_base_commit(
    parent_root: &ProjectRootPath,
    base: Option<&BaseRevision>,
) -> AppResult<String> {
    const OPERATION: &str = "workbench_create";
    let base_ref = base.map_or("HEAD", |base| base.0.as_str());
    if base_ref.trim().is_empty() || base_ref.starts_with('-') {
        return Err(AppError::invalid(
            OPERATION,
            format!("invalid base_ref '{base_ref}' for parent root {parent_root}"),
        ));
    }
    let commit_ref = format!("{base_ref}^{{commit}}");
    let output = run_git(parent_root, &["rev-parse", "--verify", &commit_ref])
        .await
        .map_err(|error| AppError::internal_message(OPERATION, error.clone(), anyhow!(error)))?;
    if !output.status.success() {
        return Err(AppError::invalid(
            OPERATION,
            format!(
                "base_ref '{}' does not resolve to a commit in parent root {}: {}",
                base_ref,
                parent_root,
                git_output_message(&output)
            ),
        ));
    }
    let commit = String::from_utf8(output.stdout).map_err(|error| {
        AppError::internal_message(
            OPERATION,
            format!("git rev-parse for parent root {parent_root} returned non-UTF-8 output"),
            error,
        )
    })?;
    let commit = commit.trim();
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::internal_message(
            OPERATION,
            format!("git rev-parse returned invalid commit SHA '{commit}' for {parent_root}"),
            anyhow!("invalid full commit SHA"),
        ));
    }
    Ok(commit.to_owned())
}

async fn git_worktree_remove(
    parent_root: &ProjectRootPath,
    worktree_root: &ProjectRootPath,
) -> Result<(), String> {
    let output = run_git(parent_root, &["worktree", "remove", &worktree_root.0]).await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree remove failed for parent root {} worktree {}: {}",
        parent_root,
        worktree_root,
        git_output_message(&output)
    ))
}

async fn git_status_porcelain(worktree_root: &ProjectRootPath) -> Result<String, String> {
    let output = run_git(
        worktree_root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "git status failed for worktree root {}: {}",
            worktree_root,
            git_output_message(&output)
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("git status output was not valid UTF-8: {error}"))
}

async fn rollback_created_worktrees(
    created: &[WorkbenchRoot],
    branch: &GitBranchName,
) -> Option<String> {
    let mut failures = Vec::new();
    for root in created.iter().rev() {
        match run_git(
            &root.parent_root,
            &["worktree", "remove", "--force", &root.worktree_root.0],
        )
        .await
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "rollback git worktree remove --force failed for {}: {}",
                root.worktree_root,
                git_output_message(&output)
            )),
            Err(error) => failures.push(format!(
                "rollback git worktree remove --force failed for {}: {}",
                root.worktree_root, error
            )),
        }
        // `git worktree add -b` created this branch in the parent repo;
        // delete it so retrying the identical create passes the
        // branch-exists preflight. The branch still points at the parent
        // HEAD (no work happened), so `-D` cannot lose anything.
        match run_git(&root.parent_root, &["branch", "-D", &branch.0]).await {
            Ok(output) if output.status.success() => {}
            Ok(output) => failures.push(format!(
                "rollback git branch -D {} failed for {}: {}",
                branch,
                root.parent_root,
                git_output_message(&output)
            )),
            Err(error) => failures.push(format!(
                "rollback git branch -D {} failed for {}: {}",
                branch, root.parent_root, error
            )),
        }
    }
    if failures.is_empty() {
        None
    } else {
        Some(failures.join("; "))
    }
}

async fn git_worktree_prune(parent_root: &ProjectRootPath) -> Result<(), String> {
    let output = run_git(parent_root, &["worktree", "prune"]).await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree prune failed for parent root {}: {}",
        parent_root,
        git_output_message(&output)
    ))
}

async fn cleanup_reviews_for_deleted_project(
    review_registry: &ReviewRegistryHandle,
    project_id: &ProjectId,
) {
    match review_registry.delete_for_project(project_id.clone()).await {
        Ok(removed) if removed.is_empty() => {}
        Ok(removed) => tracing::info!(
            project_id = %project_id,
            review_count = removed.len(),
            review_ids = ?removed,
            "deleted persisted reviews referencing removed project"
        ),
        Err(error) => tracing::warn!(
            project_id = %project_id,
            error = %error,
            "failed to delete persisted reviews referencing removed project"
        ),
    }
}

fn append_rollback_message(error: String, rollback_message: Option<String>) -> String {
    match rollback_message {
        Some(rollback_message) => format!("{error}; rollback failures: {rollback_message}"),
        None => error,
    }
}

async fn run_git(root: &ProjectRootPath, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(&root.0)
        .args(args)
        .output()
        .await
        .map_err(|error| {
            format!(
                "failed to run git -C {} {}: {}",
                root,
                args.join(" "),
                error
            )
        })
}

fn git_output_message(output: &std::process::Output) -> String {
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_owned());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!(
        "exit code {}; stdout: {}; stderr: {}",
        code,
        stdout.trim(),
        stderr.trim()
    )
}

async fn project_exists(
    project_store: &Arc<Mutex<ProjectStore>>,
    project_id: &ProjectId,
    operation: &'static str,
) -> AppResult<bool> {
    let projects = project_store
        .lock()
        .await
        .list()
        .map_err(|error| project_store_error(operation, error))?;
    Ok(projects
        .into_iter()
        .any(|project| project.id == *project_id))
}

fn project_store_error(operation: &'static str, error: ProjectStoreError) -> AppError {
    match error {
        ProjectStoreError::NotFound(message) => AppError::not_found(operation, message),
        ProjectStoreError::InvalidInput(message) => AppError::invalid(operation, message),
        ProjectStoreError::Conflict(message) => AppError::conflict(operation, message),
        ProjectStoreError::InvalidStore(message) | ProjectStoreError::Internal(message) => {
            AppError::internal_message(operation, message.clone(), anyhow!(message))
        }
    }
}

fn custom_agent_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing custom agent") {
        AppError::not_found(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid custom agent store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn steering_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing steering") {
        AppError::not_found(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid steering store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn skill_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing skill") {
        AppError::not_found(operation, error)
    } else if error.contains("duplicate directory name") {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid skills index") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn mcp_server_store_error(operation: &'static str, error: String) -> AppError {
    if error.contains("missing MCP server") {
        AppError::not_found(operation, error)
    } else if error.contains("duplicate name") {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid MCP server store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn referenced_team_member_delete_message(
    resource_kind: &str,
    resource_id: &impl std::fmt::Display,
    resource_name: Option<&str>,
    snapshot: &TeamRegistrySnapshot,
    referenced_member_ids: &[TeamMemberId],
) -> String {
    let resource = match resource_name {
        Some(name) => format!(r#"{resource_kind} "{name}""#),
        None => format!("{resource_kind} {resource_id}"),
    };
    let Some(first_member_id) = referenced_member_ids.first() else {
        return format!("cannot delete {resource} while referenced by a team member");
    };
    let message = if let Some(member) = snapshot
        .members
        .iter()
        .find(|member| member.id == *first_member_id)
    {
        if let Some(team) = snapshot.teams.iter().find(|team| team.id == member.team_id) {
            format!(
                r#"cannot delete {resource} while referenced by team member "{}" in team "{}""#,
                member.name, team.name
            )
        } else {
            format!(
                r#"cannot delete {resource} while referenced by team member "{}" in team {}"#,
                member.name, member.team_id
            )
        }
    } else {
        format!("cannot delete {resource} while referenced by team member {first_member_id}")
    };
    let remaining = referenced_member_ids.len().saturating_sub(1);
    if remaining > 0 {
        format!("{message} (and {remaining} more)")
    } else {
        message
    }
}

fn team_member_activation_error(operation: &'static str, error: String) -> AppError {
    if error.starts_with("conflict:") || error.contains("activation is already in progress") {
        AppError::conflict(operation, error)
    } else if error.starts_with("failed to load sessions")
        || error.starts_with("Failed ")
        || error.starts_with("Invalid agent teams store")
        || error.contains("references missing team")
        || error.contains("bound to missing agent")
    {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else if error.starts_with("cannot resume") || error.contains("agent backend is closed") {
        AppError::conflict(operation, error)
    } else {
        AppError::invalid(operation, error)
    }
}

fn team_registry_error(operation: &'static str, error: String) -> AppError {
    if error.contains("references missing custom agent")
        || error.contains("references missing project")
    {
        AppError::conflict(operation, error)
    } else if error.contains("missing") {
        AppError::not_found(operation, error)
    } else if error.contains("already")
        || error.contains("active manager")
        || error.contains("live-bound")
        || error.starts_with("conflict:")
    {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed ") || error.starts_with("Invalid agent teams store") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn session_store_error(operation: &'static str, error: String) -> AppError {
    if error.starts_with("Failed ") {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

fn project_command_error(operation: &'static str, error: String) -> AppError {
    if error.starts_with("No unstaged diff exists") {
        AppError::conflict(operation, error)
    } else if error.starts_with("Failed to run git")
        || error.starts_with("git ")
        || error.starts_with("git output was not valid UTF-8")
        || error.starts_with("Failed to read untracked file")
        || error.starts_with("invalid diff header")
        || error.starts_with("invalid hunk")
        || error.starts_with("missing old range in hunk header")
        || error.starts_with("missing new range in hunk header")
        || error.contains("appeared before file in git diff")
    {
        AppError::internal_message(operation, error.clone(), anyhow!(error))
    } else {
        AppError::invalid(operation, error)
    }
}

pub fn spawn_host() -> HostHandle {
    let session_path = SessionStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default session store path: {err}"));
    let project_path = ProjectStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default project store path: {err}"));
    let agent_team_path = AgentTeamsStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default agent teams store path: {err}"));
    let review_path = ReviewStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default review store path: {err}"));
    let settings_path = HostSettingsStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default settings store path: {err}"));
    let agents_view_preferences_path =
        AgentsViewPreferencesStore::default_path().unwrap_or_else(|err| {
            panic!("failed to resolve default agents view preferences store path: {err}")
        });
    let custom_agent_path = CustomAgentStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default custom agent store path: {err}"));
    let mcp_server_path = McpServerStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default MCP server store path: {err}"));
    let steering_path = SteeringStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default steering store path: {err}"));
    let skills_index_path = SkillStore::default_index_path()
        .unwrap_or_else(|err| panic!("failed to resolve default skills index path: {err}"));
    let skills_root_dir = SkillStore::default_root_dir()
        .unwrap_or_else(|err| panic!("failed to resolve default skills root dir: {err}"));
    let mobile_pairings_path = MobilePairingsStore::default_path().unwrap_or_else(|err| {
        panic!("failed to resolve default mobile pairings store path: {err}")
    });
    let workflow_runs_path = WorkflowRunStore::default_path()
        .unwrap_or_else(|err| panic!("failed to resolve default workflow run store path: {err}"));
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            agent_team: agent_team_path,
            review: review_path,
            settings: settings_path,
            agents_view_preferences: agents_view_preferences_path,
            custom_agent: custom_agent_path,
            mcp_server: mcp_server_path,
            steering: steering_path,
            skills_index: skills_index_path,
            skills_root_dir,
            mobile_pairings: mobile_pairings_path,
            workflow_runs: workflow_runs_path,
        },
        false,
        HostRuntimeConfig::default(),
    )
    .unwrap_or_else(|err| panic!("failed to initialize host stores: {err}"))
}

pub fn spawn_host_with_session_store(path: PathBuf) -> Result<HostHandle, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Session store path has no parent: {}", path.display()))?;
    let project_path = parent.join("projects.json");
    let settings_path = parent.join("settings.json");
    spawn_host_with_store_paths_and_runtime_config(
        path,
        project_path,
        settings_path,
        HostRuntimeConfig::default(),
    )
}

pub fn spawn_host_with_store_paths(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
) -> Result<HostHandle, String> {
    spawn_host_with_store_paths_and_runtime_config(
        session_path,
        project_path,
        settings_path,
        HostRuntimeConfig::default(),
    )
}

pub fn spawn_host_with_store_paths_and_runtime_config(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
    runtime_config: HostRuntimeConfig,
) -> Result<HostHandle, String> {
    let parent = session_path
        .parent()
        .map(|path| path.to_path_buf())
        .ok_or_else(|| {
            format!(
                "Session store path has no parent: {}",
                session_path.display()
            )
        })?;
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            agent_team: parent.join("agent_teams.json"),
            review: parent.join("reviews.json"),
            settings: settings_path,
            agents_view_preferences: parent.join("agents_view_preferences.json"),
            custom_agent: parent.join("custom_agents.json"),
            mcp_server: parent.join("mcp_servers.json"),
            steering: parent.join("steering.json"),
            skills_index: parent.join("skills.json"),
            skills_root_dir: parent.join("skills"),
            mobile_pairings: MobilePairingsStore::path_for_store_parent(&parent),
            workflow_runs: parent.join("workflow_runs.json"),
        },
        false,
        runtime_config,
    )
}

/// Spawn a host that uses MockBackend for all agent spawns (for tests).
pub fn spawn_host_with_mock_backend(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
) -> Result<HostHandle, String> {
    // The mock backend is test-only, so skip the real `<cli> --version` /
    // codex-model-discovery probing by default — it costs several seconds per
    // host spawn and tests don't depend on the host's installed-CLI
    // detection. Tests that *do* assert on probe output use
    // `spawn_host_with_mock_backend_and_runtime_config` with the flag cleared.
    spawn_host_with_mock_backend_and_runtime_config(
        session_path,
        project_path,
        settings_path,
        HostRuntimeConfig {
            skip_real_backend_probe: true,
            ..HostRuntimeConfig::default()
        },
    )
}

pub fn spawn_host_with_mock_backend_and_runtime_config(
    session_path: PathBuf,
    project_path: PathBuf,
    settings_path: PathBuf,
    runtime_config: HostRuntimeConfig,
) -> Result<HostHandle, String> {
    let parent = session_path
        .parent()
        .map(|path| path.to_path_buf())
        .ok_or_else(|| {
            format!(
                "Session store path has no parent: {}",
                session_path.display()
            )
        })?;
    spawn_host_inner(
        HostStorePaths {
            session: session_path,
            project: project_path,
            agent_team: parent.join("agent_teams.json"),
            review: parent.join("reviews.json"),
            settings: settings_path,
            agents_view_preferences: parent.join("agents_view_preferences.json"),
            custom_agent: parent.join("custom_agents.json"),
            mcp_server: parent.join("mcp_servers.json"),
            steering: parent.join("steering.json"),
            skills_index: parent.join("skills.json"),
            skills_root_dir: parent.join("skills"),
            mobile_pairings: MobilePairingsStore::path_for_store_parent(&parent),
            workflow_runs: parent.join("workflow_runs.json"),
        },
        true,
        runtime_config,
    )
}

fn spawn_host_inner(
    paths: HostStorePaths,
    use_mock_backend: bool,
    runtime_config: HostRuntimeConfig,
) -> Result<HostHandle, String> {
    let antigravity_conversations_dir =
        crate::backend::antigravity::resolve_antigravity_conversations_dir(
            runtime_config.antigravity_conversations_dir.as_deref(),
        )?;
    let (session_store, purged_gemini_session_ids) =
        SessionStore::load_with_migration(paths.session)?;
    let project_store = ProjectStore::load(paths.project)?;
    let initial_projects = project_store.list()?;
    let workflow_catalog = WorkflowCatalog::discover(&initial_projects);
    let workflow_locations = workflow_catalog_locations(&initial_projects);
    let workflow_watch_targets = workflow_watch_dirs(&initial_projects);
    let workflow_run_store = WorkflowRunStore::load(paths.workflow_runs)?;
    let review_store = ReviewStore::load(paths.review)?;
    let settings_store = HostSettingsStore::load(paths.settings)?;
    let agents_view_preferences_store = runtime_config
        .agents_view_preferences_primary
        .then(|| AgentsViewPreferencesStore::load(paths.agents_view_preferences));
    let host_settings = settings_store.get()?;
    let initial_mobile_settings = host_settings.clone();
    let (activity_summary_settings_tx, _activity_summary_settings_rx) =
        watch::channel(ActivitySummarySettingsSignal {
            enabled: host_settings
                .background_agent_features
                .agent_activity_summaries,
            epoch: 0,
        });
    let (supervisor_settings_tx, _supervisor_settings_rx) =
        watch::channel(SupervisorSettingsSignal {
            settings: host_settings.supervisor,
            epoch: 0,
        });
    let mobile_pairings_store = MobilePairingsStore::load(paths.mobile_pairings)?;
    let custom_agent_store = CustomAgentStore::load(paths.custom_agent)?;
    let (role_preset_ids, personality_preset_ids) = team_preset_validation_refs();
    let team_refs = AgentTeamValidationRefs {
        custom_agent_ids: custom_agent_store
            .list()?
            .into_iter()
            .map(|custom_agent| custom_agent.id)
            .collect(),
        project_ids: initial_projects
            .iter()
            .map(|project| project.id.clone())
            .collect(),
        enabled_backend_kinds: host_settings.enabled_backends.iter().copied().collect(),
        role_preset_ids,
        personality_preset_ids,
        legacy_backend_kind: host_settings.default_backend,
        purged_gemini_session_ids,
    };
    let team_store = AgentTeamsStore::load(paths.agent_team, &team_refs)?;
    let project_store = Arc::new(Mutex::new(project_store));
    let mcp_server_store = McpServerStore::load(paths.mcp_server)?;
    let steering_store = SteeringStore::load(paths.steering)?;
    let skill_store = SkillStore::load(paths.skills_index, paths.skills_root_dir)?;
    let (sub_agent_spawn_tx, sub_agent_spawn_rx) =
        mpsc::unbounded_channel::<HostSubAgentSpawnRequest>();
    let (capacity_tx, capacity_rx): (HostCapacityTx, HostCapacityRx) = mpsc::unbounded_channel();
    let (mobile_access_tx, mobile_access_rx) = mpsc::unbounded_channel::<MobileAccessCommand>();
    let mobile_access = MobileAccessHandle::new(mobile_access_tx.clone());
    let (review_delivery_tx, review_delivery_rx) = mpsc::channel::<ReviewDeliveryRequest>(64);
    let (review_ai_spawn_tx, review_ai_spawn_rx) = mpsc::channel::<ReviewAiSpawnRequest>(16);
    let (review_project_update_tx, review_project_update_rx) =
        mpsc::unbounded_channel::<ProjectId>();
    let review_registry = ReviewRegistry::spawn(
        review_store,
        Arc::clone(&project_store),
        review_delivery_tx,
        review_ai_spawn_tx,
        review_project_update_tx,
    )?;
    let debug_mcp = match crate::debug_mcp::start_server(runtime_config.debug_mcp_bind_addr) {
        Ok(handle) => handle,
        Err(err) if runtime_config.debug_mcp_bind_addr.is_none() => {
            tracing::warn!(
                "debug MCP server unavailable; continuing without it: {}",
                err
            );
            crate::debug_mcp::DebugMcpHandle { url: String::new() }
        }
        Err(err) => return Err(err),
    };

    // Create the host handle first so we can pass it to the agent-control MCP
    // server. The MCP server runs on its own thread and accesses the host
    // through the cloned handle.
    let agent_control_mcp_placeholder = AgentControlMcpHandle::disabled();
    let config_mcp_placeholder = ConfigMcpHandle { url: String::new() };
    let review_mcp_placeholder = ReviewMcpHandle { url: String::new() };
    let workflow_mcp_placeholder = WorkflowMcpHandle { url: String::new() };
    let (workflow_signal_tx, workflow_signal_rx) =
        mpsc::channel::<WorkflowCatalogSignal>(crate::workflows::watch::workflow_signal_capacity());
    let workflow_watcher =
        crate::workflows::watch::spawn_workflow_watcher(workflow_watch_targets, workflow_signal_tx);
    let (spawn_operation_tx, spawn_operation_rx) = mpsc::channel(SPAWN_OPERATION_QUEUE_CAPACITY);
    let spawn_operation_cancel = CancellationToken::new();
    let (spawn_operation_shutdown_complete, _) = tokio::sync::watch::channel(false);
    #[cfg(feature = "test-support")]
    let spawn_operation_completion_test_gate = Arc::new(StdMutex::new(None));
    #[cfg(feature = "test-support")]
    let spawn_operation_start_test_gate = Arc::new(StdMutex::new(None));
    #[cfg(feature = "test-support")]
    let spawn_operation_drain_test_gate = Arc::new(StdMutex::new(None));
    #[cfg(feature = "test-support")]
    let spawn_operation_publication_test_gate = Arc::new(StdMutex::new(None));
    let spawn_operations = Arc::new(SpawnOperationOwner {
        cancel: spawn_operation_cancel.clone(),
        worker: StdMutex::new(None),
        shutdown: StdMutex::new(SpawnOperationShutdown { started: false }),
        shutdown_complete: spawn_operation_shutdown_complete,
        #[cfg(feature = "test-support")]
        completion_test_gate: Arc::clone(&spawn_operation_completion_test_gate),
        #[cfg(feature = "test-support")]
        start_test_gate: Arc::clone(&spawn_operation_start_test_gate),
        #[cfg(feature = "test-support")]
        drain_test_gate: Arc::clone(&spawn_operation_drain_test_gate),
        #[cfg(feature = "test-support")]
        publication_test_gate: Arc::clone(&spawn_operation_publication_test_gate),
    });
    let spawn_operation_handle = SpawnOperationHandle {
        tx: spawn_operation_tx,
        owner: Arc::downgrade(&spawn_operations),
    };
    let host = HostHandle {
        state: Arc::new(Mutex::new(HostState {
            registry: AgentRegistry::new(),
            review_registry,
            team_registry: TeamRegistryHandle::spawn(team_store),
            project_store,
            settings_store: Arc::new(Mutex::new(settings_store)),
            agents_view_preferences_store: agents_view_preferences_store
                .map(|store| Arc::new(Mutex::new(store))),
            session_store: Arc::new(Mutex::new(session_store)),
            custom_agent_store: Arc::new(Mutex::new(custom_agent_store)),
            mcp_server_store: Arc::new(Mutex::new(mcp_server_store)),
            steering_store: Arc::new(Mutex::new(steering_store)),
            skill_store: Arc::new(Mutex::new(skill_store)),
            agent_sessions: HashMap::new(),
            pending_agent_sessions: HashMap::new(),
            agent_visibility: AgentVisibilityRegistry::default(),
            agent_activity_summaries: HashMap::new(),
            closed_agent_usage_snapshots: HashMap::new(),
            activity_summary_epoch: 0,
            supervisor_epoch: 0,
            supervisor_settings_tx,
            activity_summary_settings_tx,
            sub_agent_spawn_tx,
            capacity_tx: capacity_tx.clone(),
            use_mock_backend,
            debug_mcp,
            agent_control_mcp: agent_control_mcp_placeholder,
            config_mcp: config_mcp_placeholder,
            review_mcp: review_mcp_placeholder,
            workflow_mcp: workflow_mcp_placeholder,
            workflow_watcher,
            workflow_catalog,
            workflow_locations,
            workflow_run_store,
            mobile_access: mobile_access.clone(),
            codex_session_schema: CodexSessionSchemaState::Pending,
            kiro_session_schema: KiroSessionSchemaState::Pending,
            hermes_session_schema: HermesSessionSchemaState::Pending,
            hermes_launch_profiles: Vec::new(),
            backend_config_snapshots: Vec::new(),
            backend_native_settings_snapshots: Vec::new(),
            backend_capacity: initial_backend_capacity_snapshots(),
            antigravity_conversations_dir,
            codex_probe_program: runtime_config.codex_probe_program.clone(),
            kiro_probe_program: runtime_config.kiro_probe_program.clone(),
            kiro_probe_workspace_root: runtime_config.kiro_probe_workspace_root.clone(),
            skip_real_backend_probe: runtime_config.skip_real_backend_probe,
            host_streams: HashMap::new(),
            project_streams: HashMap::new(),
            terminal_streams: HashMap::new(),
            browse_streams: HashMap::new(),
            workbench_parent_locks: HashMap::new(),
            project_search_ids: HashMap::new(),
            code_intel_routers: HashMap::new(),
            removing_projects: HashSet::new(),
            #[cfg(feature = "test-support")]
            agent_name_test_gate: None,
            #[cfg(feature = "test-support")]
            session_schema_probe_count: 0,
        })),
        workflow_save_lock: Arc::new(Mutex::new(())),
        backend_setup_refresh_lock: Arc::new(Mutex::new(())),
        session_schema_refresh_lock: Arc::new(Mutex::new(())),
        spawn_operations: spawn_operation_handle.clone(),
        spawn_operation_owner: Some(Arc::clone(&spawn_operations)),
    };

    let spawn_operation_worker = spawn_host_spawn_operation_task(
        WeakHostHandle {
            state: Arc::downgrade(&host.state),
            workflow_save_lock: Arc::downgrade(&host.workflow_save_lock),
            backend_setup_refresh_lock: Arc::downgrade(&host.backend_setup_refresh_lock),
            session_schema_refresh_lock: Arc::downgrade(&host.session_schema_refresh_lock),
            spawn_operations: spawn_operation_handle,
        },
        spawn_operation_rx,
        spawn_operation_cancel,
        #[cfg(feature = "test-support")]
        spawn_operation_completion_test_gate,
        #[cfg(feature = "test-support")]
        spawn_operation_start_test_gate,
        #[cfg(feature = "test-support")]
        spawn_operation_drain_test_gate,
        #[cfg(feature = "test-support")]
        spawn_operation_publication_test_gate,
    );
    *spawn_operations
        .worker
        .lock()
        .expect("spawn operation worker mutex poisoned") = Some(spawn_operation_worker);

    spawn_mobile_access_actor(
        host.clone(),
        mobile_access_tx,
        mobile_access_rx,
        MobileAccessInit {
            pairings_store: mobile_pairings_store,
            initial_settings: initial_mobile_settings,
            pairing_ttl: runtime_config
                .mobile_pairing_ttl
                .unwrap_or(crate::mobile_access::DEFAULT_PAIRING_TTL),
            managed_service_base_url: runtime_config.mobile_managed_service_base_url,
        },
    )?;

    spawn_host_capacity_task(host.clone(), capacity_rx);

    let agent_control_mcp = match crate::agent_control_mcp::start_server(
        runtime_config.agent_control_mcp_bind_addr,
        host.clone(),
    ) {
        Ok(handle) => handle,
        Err(err) if runtime_config.agent_control_mcp_bind_addr.is_none() => {
            tracing::warn!(
                "agent-control MCP server unavailable; continuing without it: {}",
                err
            );
            AgentControlMcpHandle::disabled()
        }
        Err(err) => return Err(err),
    };

    // Store the real handle now that the server is running. We just created
    // the Arc<Mutex<HostState>> above and no async tasks are holding it yet,
    // so try_lock always succeeds.
    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .agent_control_mcp = agent_control_mcp;

    let config_mcp = match crate::config_mcp::start_server(None, host.clone()) {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!(
                "config MCP server unavailable; continuing without it: {}",
                err
            );
            ConfigMcpHandle { url: String::new() }
        }
    };
    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .config_mcp = config_mcp;

    let review_mcp =
        match crate::review_mcp::start_server(runtime_config.review_mcp_bind_addr, host.clone()) {
            Ok(handle) => handle,
            Err(err) if runtime_config.review_mcp_bind_addr.is_none() => {
                tracing::warn!(
                    "review MCP server unavailable; continuing without it: {}",
                    err
                );
                ReviewMcpHandle { url: String::new() }
            }
            Err(err) => return Err(err),
        };

    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .review_mcp = review_mcp;

    let workflow_mcp = match crate::workflows::mcp::start_server(
        runtime_config.workflow_mcp_bind_addr,
        host.clone(),
    ) {
        Ok(handle) => handle,
        Err(err) if runtime_config.workflow_mcp_bind_addr.is_none() => {
            tracing::warn!(
                "workflow MCP server unavailable; continuing without it: {}",
                err
            );
            WorkflowMcpHandle { url: String::new() }
        }
        Err(err) => return Err(err),
    };
    host.state
        .try_lock()
        .expect("newly created host state must be unlocked")
        .workflow_mcp = workflow_mcp;

    spawn_host_sub_agent_task(host.clone(), sub_agent_spawn_rx);
    spawn_host_review_delivery_task(host.clone(), review_delivery_rx);
    spawn_host_review_ai_task(host.clone(), review_ai_spawn_rx);
    spawn_host_review_project_update_task(host.clone(), review_project_update_rx);
    spawn_host_workflow_catalog_task(host.clone(), workflow_signal_rx);
    spawn_host_team_status_task(host.clone());
    spawn_task_token_usage_task(host.clone());
    spawn_agent_activity_summary_task(host.clone());
    #[cfg(not(test))]
    spawn_agent_supervisor_task(host.clone());
    #[cfg(test)]
    if runtime_config.start_agent_supervisor_worker {
        spawn_agent_supervisor_task(host.clone());
    }

    Ok(host)
}

fn spawn_task_token_usage_task(host: HostHandle) {
    let worker = async move {
        let mut status_rx = host.subscribe_agent_status_changes().await;
        loop {
            if status_rx.changed().await.is_err() {
                break;
            }
            host.fan_out_task_token_usages().await;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    if let Err(err) = std::thread::Builder::new()
        .name("tyde-task-token-usage".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to build task-token-usage runtime"
                    );
                    return;
                }
            };
            runtime.block_on(worker);
        })
    {
        tracing::error!(
            error = %err,
            "failed to spawn task-token-usage worker thread"
        );
    }
}

fn spawn_host_team_status_task(host: HostHandle) {
    let worker = async move {
        let mut status_rx = host.subscribe_agent_status_changes().await;
        let mut last_seen = HashMap::<AgentId, u64>::new();
        loop {
            if status_rx.changed().await.is_err() {
                break;
            }
            let entries = {
                let state = host.state.lock().await;
                state
                    .registry
                    .agent_ids()
                    .into_iter()
                    .filter_map(|agent_id| {
                        state
                            .registry
                            .agent_status_handle(&agent_id)
                            .map(|status_handle| (agent_id, status_handle))
                    })
                    .collect::<Vec<_>>()
            };
            let live_agent_ids = entries
                .iter()
                .map(|(agent_id, _)| agent_id.clone())
                .collect::<HashSet<_>>();
            last_seen.retain(|agent_id, _| live_agent_ids.contains(agent_id));

            for (agent_id, status_handle) in entries {
                let status = status_handle.snapshot().await;
                if last_seen.get(&agent_id).copied() == Some(status.activity_counter) {
                    continue;
                }
                last_seen.insert(agent_id.clone(), status.activity_counter);
                let registry = { host.state.lock().await.team_registry.clone() };
                let result = if status.terminated {
                    registry.clear_binding_by_agent(agent_id.clone()).await
                } else if status.is_plan_approval_pending() {
                    registry
                        .record_agent_activity(agent_id.clone(), AgentControlStatus::Thinking)
                        .await
                } else {
                    registry
                        .record_agent_activity(agent_id.clone(), status.status())
                        .await
                };
                match result {
                    Ok(events) => host.fan_out_team_registry_events(events).await,
                    Err(error) => {
                        tracing::warn!(
                            agent_id = %agent_id,
                            error = %error,
                            "failed to update team member binding from agent status"
                        );
                    }
                }
            }
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    if let Err(err) = std::thread::Builder::new()
        .name("tyde-host-team-status".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to build host team-status runtime"
                    );
                    return;
                }
            };
            runtime.block_on(worker);
        })
    {
        tracing::error!(
            error = %err,
            "failed to spawn host team-status worker thread"
        );
    }
}

fn spawn_agent_activity_summary_task(host: HostHandle) {
    let worker = async move {
        let mut status_rx = host.subscribe_agent_status_changes().await;
        let mut settings_rx = host.activity_summary_settings_receiver().await;
        let (task_event_tx, mut task_event_rx) =
            mpsc::unbounded_channel::<ActivitySummaryTaskEvent>();
        let semaphore = Arc::new(Semaphore::new(1));
        let mut entries = HashMap::<AgentId, ActivitySummarySchedulerEntry>::new();

        loop {
            let current_settings = *settings_rx.borrow();
            let next_due = if current_settings.enabled {
                entries
                    .values()
                    .filter_map(|entry| (!entry.in_flight).then_some(entry.pending_due).flatten())
                    .min()
            } else {
                None
            };
            let sleep_duration = next_due
                .map(|due| due.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| Duration::from_secs(3600));
            let sleep = tokio::time::sleep(sleep_duration);
            tokio::pin!(sleep);

            tokio::select! {
                _ = &mut sleep, if next_due.is_some() => {
                    start_due_activity_summary_calls(
                        &host,
                        &mut entries,
                        current_settings,
                        Arc::clone(&semaphore),
                        task_event_tx.clone(),
                    )
                    .await;
                }
                changed = status_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let settings = *settings_rx.borrow();
                    if settings.enabled {
                        observe_activity_summary_agents(&host, &mut entries).await;
                    }
                }
                changed = settings_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let settings = *settings_rx.borrow();
                    if settings.enabled {
                        observe_activity_summary_agents(&host, &mut entries).await;
                    } else {
                        for entry in entries.values_mut() {
                            entry.pending_due = None;
                            entry.queued_final_refresh = false;
                            entry.in_flight = false;
                        }
                    }
                }
                Some(event) = task_event_rx.recv() => {
                    let settings = *settings_rx.borrow();
                    match event {
                        ActivitySummaryTaskEvent::Started(started) => {
                            begin_activity_summary_call(&host, &mut entries, started, settings).await;
                        }
                        ActivitySummaryTaskEvent::Finished(result) => {
                            finish_activity_summary_call(&host, &mut entries, result, settings).await;
                        }
                    }
                }
            }
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    if let Err(err) = std::thread::Builder::new()
        .name("tyde-agent-activity-summaries".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to build agent activity summary runtime"
                    );
                    return;
                }
            };
            runtime.block_on(worker);
        })
    {
        tracing::error!(
            error = %err,
            "failed to spawn agent activity summary worker thread"
        );
    }
}

/// Watches supervised agents with one scheduler-owned inactivity phase machine.
/// Model verdicts and compaction work run in bounded detached tasks, but this
/// task alone owns generations, phases, idle timestamps, and deadlines.
fn spawn_agent_supervisor_task(host: HostHandle) {
    let worker = async move {
        let mut status_rx = host.subscribe_agent_status_changes().await;
        let mut settings_rx = host.supervisor_settings_receiver().await;
        let (verdict_tx, mut verdict_rx) = mpsc::unbounded_channel::<SupervisorVerdictTaskEvent>();
        let (compaction_tx, mut compaction_rx) =
            mpsc::unbounded_channel::<SupervisorCompactionTaskEvent>();
        let semaphore = Arc::new(Semaphore::new(1));
        let mut verdict_task_state = SupervisorVerdictTaskState::default();
        let mut entries = HashMap::<AgentId, SupervisorSchedulerEntry>::new();
        let mut last_seen_settings = *settings_rx.borrow();

        if last_seen_settings.settings.enabled {
            observe_supervised_agents(&host, &mut entries).await;
        }

        loop {
            let current_settings = *settings_rx.borrow();
            let next_deadline = supervisor_next_deadline(
                &entries,
                current_settings,
                verdict_task_state.is_active(),
            );
            let sleep_until = next_deadline.unwrap_or_else(|| {
                Instant::now()
                    .checked_add(Duration::from_secs(86_400))
                    .unwrap_or_else(Instant::now)
            });
            let deadline_sleep = tokio::time::sleep_until(sleep_until.into());
            tokio::pin!(deadline_sleep);

            tokio::select! {
                changed = status_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let supervisor_enabled = settings_rx.borrow().settings.enabled;
                    if supervisor_enabled {
                        observe_supervised_agents(&host, &mut entries).await;
                    }
                }
                changed = settings_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let previous = last_seen_settings;
                    let current = *settings_rx.borrow();
                    last_seen_settings = current;
                    apply_supervisor_settings_change(
                        &host,
                        &mut entries,
                        previous,
                        current,
                    )
                    .await;
                }
                Some(event) = verdict_rx.recv() => {
                    if !verdict_task_state.finish(event.task_id) {
                        tracing::warn!(
                            task_id = event.task_id,
                            "dropping an unowned supervisor verdict task result"
                        );
                        continue;
                    }
                    let current = *settings_rx.borrow();
                    accept_supervision_verdict_result(
                        &host,
                        &mut entries,
                        current,
                        event.result,
                        Instant::now(),
                    )
                    .await;
                }
                Some(event) = compaction_rx.recv() => {
                    match event {
                        SupervisorCompactionTaskEvent::Started {
                            agent_id,
                            activity_counter,
                            accepted,
                        } => {
                            let Some(entry) = entries.get_mut(&agent_id) else {
                                continue;
                            };
                            if entry.last_activity_counter != activity_counter
                                || !matches!(&entry.phase, SupervisorPhase::CompactionPending { .. })
                            {
                                continue;
                            }
                            if accepted {
                                entry.phase = SupervisorPhase::Compacting;
                                tracing::info!(
                                    agent_id = %agent_id,
                                    activity_counter,
                                    "supervisor auto-compaction crossed the actor inactivity gate"
                                );
                            } else {
                                let idle_since = supervisor_phase_idle_since(&entry.phase)
                                    .unwrap_or_else(Instant::now);
                                entry.phase = SupervisorPhase::Dormant { idle_since };
                                tracing::info!(
                                    agent_id = %agent_id,
                                    activity_counter,
                                    "supervisor auto-compaction rejected after intervening activity"
                                );
                                observe_supervised_agents(&host, &mut entries).await;
                            }
                        }
                        SupervisorCompactionTaskEvent::Finished {
                            agent_id,
                            activity_counter,
                        } => {
                            if entries.get(&agent_id).is_some_and(|entry| {
                                entry.last_activity_counter == activity_counter
                                    && matches!(&entry.phase, SupervisorPhase::Compacting)
                            }) {
                                entries.remove(&agent_id);
                            }
                        }
                    }
                }
                _ = &mut deadline_sleep, if next_deadline.is_some() => {
                    process_supervisor_deadlines_from_signal(
                        &host,
                        &mut entries,
                        &settings_rx,
                        &verdict_tx,
                        &compaction_tx,
                        &semaphore,
                        &mut verdict_task_state,
                    )
                    .await;
                }
            }
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    if let Err(err) = std::thread::Builder::new()
        .name("tyde-agent-supervisor".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(error = %err, "failed to build agent supervisor runtime");
                    return;
                }
            };
            runtime.block_on(worker);
        })
    {
        tracing::error!(error = %err, "failed to spawn agent supervisor worker thread");
    }
}

async fn process_supervisor_deadlines_from_signal(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    settings_rx: &watch::Receiver<SupervisorSettingsSignal>,
    verdict_tx: &mpsc::UnboundedSender<SupervisorVerdictTaskEvent>,
    compaction_tx: &mpsc::UnboundedSender<SupervisorCompactionTaskEvent>,
    semaphore: &Arc<Semaphore>,
    verdict_task_state: &mut SupervisorVerdictTaskState,
) {
    let settings = *settings_rx.borrow();
    process_supervisor_deadlines(
        host,
        entries,
        settings,
        verdict_tx,
        compaction_tx,
        semaphore,
        verdict_task_state,
    )
    .await;
}

fn supervisor_phase_idle_since(phase: &SupervisorPhase) -> Option<Instant> {
    match phase {
        SupervisorPhase::Debouncing { idle_since }
        | SupervisorPhase::VerdictInFlight { idle_since, .. }
        | SupervisorPhase::RetryPending { idle_since, .. }
        | SupervisorPhase::FailureExhausted { idle_since, .. }
        | SupervisorPhase::DoneAuthorized { idle_since, .. }
        | SupervisorPhase::AwaitingUser { idle_since, .. }
        | SupervisorPhase::Dormant { idle_since }
        | SupervisorPhase::CompactionPending { idle_since, .. } => Some(*idle_since),
        SupervisorPhase::Active | SupervisorPhase::Compacting => None,
    }
}

fn supervised_observation_is_eligible(observation: &ActivitySummaryObservation) -> bool {
    matches!(
        observation.start.origin,
        AgentOrigin::User | AgentOrigin::SideQuestion
    ) && observation.start.parent_agent_id.is_none()
        && !observation.status.terminated
}

fn supervisor_phase_for_fresh_observation(
    status: &crate::agent::registry::AgentStatus,
    now: Instant,
) -> SupervisorPhase {
    if status.is_active() {
        SupervisorPhase::Active
    } else if status.is_plan_approval_pending() {
        SupervisorPhase::Dormant { idle_since: now }
    } else {
        SupervisorPhase::Debouncing { idle_since: now }
    }
}

fn mark_supervisor_kick_pending(status: &mut crate::agent::registry::AgentStatus) {
    status.turn_completed = false;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveRetrySettingsResult {
    Unchanged,
    FailureExhausted {
        idle_since: Instant,
        attempts_started: u8,
    },
    SettingsExhausted {
        idle_since: Instant,
        attempts_started: u8,
    },
}

fn apply_live_retry_settings(
    agent_id: &AgentId,
    entry: &mut SupervisorSchedulerEntry,
    settings: protocol::SupervisorSettings,
) -> LiveRetrySettingsResult {
    let maximum_attempts = settings.retry_attempts.saturating_add(1);
    let pending = match &entry.phase {
        SupervisorPhase::RetryPending {
            idle_since,
            baseline,
            attempts_started,
            due_at,
            last_failure_kind,
            verdict_settings: _,
        } => Some((
            *idle_since,
            baseline.clone(),
            *attempts_started,
            *due_at,
            last_failure_kind.clone(),
        )),
        SupervisorPhase::FailureExhausted {
            idle_since,
            baseline,
            attempts_started,
            retry_due_at,
            last_failure_kind,
            ..
        } => {
            if *attempts_started >= maximum_attempts {
                return LiveRetrySettingsResult::FailureExhausted {
                    idle_since: *idle_since,
                    attempts_started: *attempts_started,
                };
            }
            let Some(due_at) = retry_due_at else {
                return LiveRetrySettingsResult::Unchanged;
            };
            let idle_since = *idle_since;
            let baseline = baseline.clone();
            let attempts_started = *attempts_started;
            let due_at = *due_at;
            let last_failure_kind = *last_failure_kind;
            entry.phase = SupervisorPhase::RetryPending {
                idle_since,
                baseline,
                attempts_started,
                due_at,
                last_failure_kind: SupervisionRetryReason::Failure(last_failure_kind),
                verdict_settings: VerdictSettingsFingerprint::from(settings),
            };
            tracing::info!(
                agent_id = %agent_id,
                attempts_started,
                maximum_attempts,
                next_due = ?due_at,
                "raised retry limit resumed failure-exhausted supervision with its original backoff"
            );
            return LiveRetrySettingsResult::Unchanged;
        }
        _ => None,
    };
    let Some((idle_since, baseline, attempts_started, due_at, reason)) = pending else {
        return LiveRetrySettingsResult::Unchanged;
    };
    if attempts_started >= maximum_attempts {
        tracing::info!(
            agent_id = %agent_id,
            attempts_started,
            maximum_attempts,
            "live retry limit exhausted pending supervision attempts"
        );
        match reason {
            SupervisionRetryReason::Failure(last_failure_kind) => {
                entry.phase = SupervisorPhase::FailureExhausted {
                    idle_since,
                    baseline,
                    attempts_started,
                    retry_due_at: Some(due_at),
                    last_failure_kind,
                };
                LiveRetrySettingsResult::FailureExhausted {
                    idle_since,
                    attempts_started,
                }
            }
            SupervisionRetryReason::SettingsChanged => LiveRetrySettingsResult::SettingsExhausted {
                idle_since,
                attempts_started,
            },
        }
    } else if let SupervisorPhase::RetryPending {
        verdict_settings, ..
    } = &mut entry.phase
    {
        *verdict_settings = VerdictSettingsFingerprint::from(settings);
        LiveRetrySettingsResult::Unchanged
    } else {
        LiveRetrySettingsResult::Unchanged
    }
}

async fn apply_supervisor_settings_change(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    previous: SupervisorSettingsSignal,
    current: SupervisorSettingsSignal,
) {
    if !current.settings.enabled {
        if previous.settings.enabled {
            tracing::info!("agent supervisor disabled; clearing inactivity state");
        }
        entries.clear();
    } else if !previous.settings.enabled {
        tracing::info!("agent supervisor enabled; starting fresh inactivity intervals");
        entries.clear();
        observe_supervised_agents(host, entries).await;
    } else {
        observe_supervised_agents(host, entries).await;
        let agent_ids = entries.keys().cloned().collect::<Vec<_>>();
        let mut failure_exhaustions = Vec::new();
        for agent_id in agent_ids {
            let Some(entry) = entries.get_mut(&agent_id) else {
                continue;
            };
            match apply_live_retry_settings(&agent_id, entry, current.settings) {
                LiveRetrySettingsResult::Unchanged => {}
                LiveRetrySettingsResult::FailureExhausted {
                    idle_since,
                    attempts_started,
                } => failure_exhaustions.push((
                    agent_id,
                    entry.last_activity_counter,
                    idle_since,
                    attempts_started,
                )),
                LiveRetrySettingsResult::SettingsExhausted { idle_since, .. } => {
                    entry.phase = SupervisorPhase::Dormant { idle_since };
                }
            }
        }
        for (agent_id, activity_counter, idle_since, attempts_started) in failure_exhaustions {
            exhaust_supervision_by_failure(
                host,
                entries,
                &agent_id,
                activity_counter,
                idle_since,
                attempts_started,
                current,
            )
            .await;
        }
    }
}

async fn observe_supervised_agents(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
) {
    let observations = host.activity_summary_observations().await;
    let eligible_ids = observations
        .iter()
        .filter(|observation| supervised_observation_is_eligible(observation))
        .map(|observation| observation.agent_id.clone())
        .collect::<HashSet<_>>();
    entries.retain(|agent_id, _| eligible_ids.contains(agent_id));

    for observation in observations
        .into_iter()
        .filter(supervised_observation_is_eligible)
    {
        let now = Instant::now();
        let status = &observation.status;
        let Some(entry) = entries.get_mut(&observation.agent_id) else {
            let phase = supervisor_phase_for_fresh_observation(status, now);
            entries.insert(
                observation.agent_id,
                SupervisorSchedulerEntry {
                    last_activity_counter: status.activity_counter,
                    phase,
                },
            );
            continue;
        };

        let activity_changed = entry.last_activity_counter != status.activity_counter;
        if activity_changed {
            if matches!(
                &entry.phase,
                SupervisorPhase::CompactionPending { .. } | SupervisorPhase::Compacting
            ) {
                continue;
            }
            if let SupervisorPhase::RetryPending {
                attempts_started, ..
            }
            | SupervisorPhase::VerdictInFlight {
                attempts_started, ..
            } = &entry.phase
            {
                tracing::info!(
                    agent_id = %observation.agent_id,
                    attempts_started,
                    old_activity_counter = entry.last_activity_counter,
                    new_activity_counter = status.activity_counter,
                    "agent activity cancelled the pending supervision generation"
                );
            }
            entry.last_activity_counter = status.activity_counter;
            entry.phase = supervisor_phase_for_fresh_observation(status, now);
            continue;
        }

        if status.is_active() {
            if !matches!(&entry.phase, SupervisorPhase::Compacting) {
                entry.phase = SupervisorPhase::Active;
            }
        } else if status.is_plan_approval_pending() {
            if !matches!(&entry.phase, SupervisorPhase::Compacting) {
                let idle_since = supervisor_phase_idle_since(&entry.phase).unwrap_or(now);
                entry.phase = SupervisorPhase::Dormant { idle_since };
            }
        } else if matches!(&entry.phase, SupervisorPhase::Active) {
            entry.phase = SupervisorPhase::Debouncing { idle_since: now };
        }
    }
}

fn supervisor_next_deadline(
    entries: &HashMap<AgentId, SupervisorSchedulerEntry>,
    settings: SupervisorSettingsSignal,
    verdict_task_in_flight: bool,
) -> Option<Instant> {
    entries
        .values()
        .filter_map(|entry| match &entry.phase {
            SupervisorPhase::Debouncing { idle_since } if !verdict_task_in_flight => {
                idle_since.checked_add(SUPERVISION_DEBOUNCE)
            }
            SupervisorPhase::RetryPending { due_at, .. } if !verdict_task_in_flight => {
                Some(*due_at)
            }
            SupervisorPhase::DoneAuthorized {
                idle_since,
                last_gate_evaluation_epoch,
                ..
            } if settings.settings.auto_compact_on_success
                && *last_gate_evaluation_epoch != Some(settings.epoch) =>
            {
                idle_since.checked_add(Duration::from_secs(u64::from(
                    settings.settings.auto_compact_inactivity_delay_seconds,
                )))
            }
            _ => None,
        })
        .min()
}

async fn process_supervisor_deadlines(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    settings: SupervisorSettingsSignal,
    verdict_tx: &mpsc::UnboundedSender<SupervisorVerdictTaskEvent>,
    compaction_tx: &mpsc::UnboundedSender<SupervisorCompactionTaskEvent>,
    semaphore: &Arc<Semaphore>,
    verdict_task_state: &mut SupervisorVerdictTaskState,
) {
    process_supervisor_deadlines_at(
        host,
        entries,
        settings,
        verdict_tx,
        compaction_tx,
        semaphore,
        verdict_task_state,
        Instant::now(),
    )
    .await;
}

async fn process_supervisor_deadlines_at(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    settings: SupervisorSettingsSignal,
    verdict_tx: &mpsc::UnboundedSender<SupervisorVerdictTaskEvent>,
    compaction_tx: &mpsc::UnboundedSender<SupervisorCompactionTaskEvent>,
    semaphore: &Arc<Semaphore>,
    verdict_task_state: &mut SupervisorVerdictTaskState,
    now: Instant,
) {
    if !verdict_task_state.is_active() {
        let due_debounces = entries
            .iter()
            .filter_map(|(agent_id, entry)| match &entry.phase {
                SupervisorPhase::Debouncing { idle_since }
                    if idle_since
                        .checked_add(SUPERVISION_DEBOUNCE)
                        .is_some_and(|due| due <= now) =>
                {
                    Some(agent_id.clone())
                }
                SupervisorPhase::RetryPending { due_at, .. } if *due_at <= now => {
                    Some(agent_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for agent_id in due_debounces {
            launch_supervision_verdict(
                host,
                entries,
                agent_id,
                settings,
                verdict_tx,
                semaphore,
                verdict_task_state,
            )
            .await;
            if verdict_task_state.is_active() {
                break;
            }
        }
    }

    let due_compactions = entries
        .iter()
        .filter_map(|(agent_id, entry)| match &entry.phase {
            SupervisorPhase::DoneAuthorized {
                idle_since,
                last_gate_evaluation_epoch,
                ..
            } if settings.settings.auto_compact_on_success
                && *last_gate_evaluation_epoch != Some(settings.epoch)
                && idle_since
                    .checked_add(Duration::from_secs(u64::from(
                        settings.settings.auto_compact_inactivity_delay_seconds,
                    )))
                    .is_some_and(|due| due <= now) =>
            {
                Some(agent_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for agent_id in due_compactions {
        launch_supervisor_auto_compaction(host, entries, agent_id, settings, compaction_tx).await;
    }
}

async fn launch_supervision_verdict(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    agent_id: AgentId,
    settings: SupervisorSettingsSignal,
    verdict_tx: &mpsc::UnboundedSender<SupervisorVerdictTaskEvent>,
    semaphore: &Arc<Semaphore>,
    verdict_task_state: &mut SupervisorVerdictTaskState,
) {
    if verdict_task_state.is_active() {
        return;
    }
    let Some(entry) = entries.get(&agent_id) else {
        return;
    };
    let (idle_since, attempts_started, pending_baseline, retry_reason, previous_verdict_settings) =
        match &entry.phase {
            SupervisorPhase::Debouncing { idle_since } => (*idle_since, 0, None, None, None),
            SupervisorPhase::RetryPending {
                idle_since,
                baseline,
                attempts_started,
                last_failure_kind,
                verdict_settings,
                ..
            } => (
                *idle_since,
                *attempts_started,
                Some(baseline.clone()),
                Some(last_failure_kind.clone()),
                Some(*verdict_settings),
            ),
            _ => return,
        };
    let activity_counter = entry.last_activity_counter;
    let Some(observation) = host.activity_summary_observation(&agent_id).await else {
        entries.remove(&agent_id);
        return;
    };
    if observation.status.activity_counter != activity_counter
        || observation.status.terminated
        || observation.status.is_active()
        || observation.status.is_plan_approval_pending()
    {
        observe_supervised_agents(host, entries).await;
        return;
    }
    let Some(context) = observation.handle.read_supervision_context().await else {
        entries.insert(
            agent_id,
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::Dormant { idle_since },
            },
        );
        return;
    };
    let Some(last_user_message) = context.last_user_message.clone() else {
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        return;
    };
    if context.cancelled_since_user_message {
        tracing::debug!(
            agent_id = %agent_id,
            "skipping supervision: user cancelled work since their last message"
        );
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        return;
    }
    let max_kicks = u32::from(settings.settings.max_kicks_per_task.max(1));
    if context.kicks_since_user_message >= max_kicks {
        tracing::info!(
            agent_id = %agent_id,
            kicks = context.kicks_since_user_message,
            "supervision kick budget exhausted; leaving the agent idle"
        );
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        return;
    }

    let session_id = observation.start.session_id.clone();
    let (task_list, session_record) = match &session_id {
        Some(session_id) => {
            let session_store = { host.state.lock().await.session_store.clone() };
            let store = session_store.lock().await;
            (store.get_task_list(session_id), store.get(session_id))
        }
        None => (None, None),
    };
    if session_record
        .as_ref()
        .is_some_and(|record| record.compacted_to_session_id.is_some())
    {
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        return;
    }
    if session_record
        .as_ref()
        .is_some_and(|record| record.compacted_from_session_id.is_some())
        && context.user_message_count <= 1
    {
        tracing::debug!(
            agent_id = %agent_id,
            "skipping supervision: post-compaction bootstrap turn"
        );
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        return;
    }

    let baseline = SupervisionBaseline {
        last_user_message: last_user_message.clone(),
        kicks_since_user_message: context.kicks_since_user_message,
        session_id,
    };
    if pending_baseline
        .as_ref()
        .is_some_and(|pending| pending != &baseline)
    {
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        return;
    }

    let use_mock_backend = host.use_mock_backend().await;
    let capacity_tx = { host.state.lock().await.capacity_tx.clone() };
    let backend_kind = observation.start.backend_kind;
    let cost_hint = settings.settings.cost_tier.as_cost_hint();
    let last_assistant_message = context.last_assistant_message;
    let last_error = context.last_error_since_user_message;
    let tx = verdict_tx.clone();
    let verdict_settings = VerdictSettingsFingerprint::from(settings.settings);
    let settings_rx = host.supervisor_settings_receiver().await;
    match observation
        .handle
        .begin_supervisor_verdict_if_inactive(activity_counter, verdict_settings, settings_rx)
        .await
    {
        SupervisorVerdictStart::Accepted => {}
        SupervisorVerdictStart::Rejected {
            reason,
            live_settings,
        } => {
            tracing::debug!(
                agent_id = %agent_id,
                ?reason,
                "supervision verdict start rejected at agent activity/settings boundary"
            );
            if live_settings != settings {
                apply_supervisor_settings_change(host, entries, settings, live_settings).await;
            } else {
                observe_supervised_agents(host, entries).await;
            }
            return;
        }
        SupervisorVerdictStart::Closed => {
            entries.remove(&agent_id);
            return;
        }
    }
    let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() else {
        return;
    };
    let Some(task_id) = verdict_task_state.reserve() else {
        drop(permit);
        return;
    };
    let attempts_started = attempts_started.saturating_add(1);
    entries.get_mut(&agent_id).expect("entry exists").phase = SupervisorPhase::VerdictInFlight {
        idle_since,
        baseline: baseline.clone(),
        attempts_started,
        verdict_settings,
    };
    tracing::info!(
        agent_id = %agent_id,
        attempt = attempts_started,
        maximum_attempts = settings.settings.retry_attempts.saturating_add(1),
        retry_reason = ?retry_reason,
        previous_verdict_settings = ?previous_verdict_settings,
        "starting agent supervision verdict attempt"
    );
    let aborted_result = SupervisorVerdictTaskResult {
        agent_id: agent_id.clone(),
        activity_counter,
        baseline: baseline.clone(),
        attempts_started,
        verdict_settings,
        result: Err(crate::agent::supervisor::SupervisionFailure {
            kind: crate::agent::supervisor::SupervisionFailureKind::BackendStream,
            message: "supervision verdict task aborted before completion".to_owned(),
        }),
    };
    let completion = SupervisorVerdictTaskCompletion {
        task_id,
        tx,
        permit: Some(permit),
        aborted: Some(aborted_result),
    };
    tokio::spawn(async move {
        wait_for_supervisor_verdict_call_test_gate(
            &agent_id,
            activity_counter,
            attempts_started,
            cost_hint,
        )
        .await;
        let request = crate::agent::supervisor::GenerateSupervisionVerdictRequest {
            verdict_agent_id: AgentId(Uuid::new_v4().to_string()),
            backend_kind,
            last_user_message,
            task_list,
            last_assistant_message,
            last_error,
            kicks_so_far: baseline.kicks_since_user_message,
            max_kicks,
            cost_hint,
            use_mock_backend,
            capacity_tx,
        };
        let result = match tokio::time::timeout(
            SUPERVISION_GENERATION_TIMEOUT,
            crate::agent::supervisor::generate_supervision_verdict(request),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(crate::agent::supervisor::SupervisionFailure {
                kind: crate::agent::supervisor::SupervisionFailureKind::Timeout,
                message: format!(
                    "supervision verdict timed out after {}",
                    generation_timeout_label(SUPERVISION_GENERATION_TIMEOUT)
                ),
            }),
        };
        completion.complete(SupervisorVerdictTaskResult {
            agent_id,
            activity_counter,
            baseline,
            attempts_started,
            verdict_settings,
            result,
        });
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SupervisionRetrySchedule {
    Pending,
    Exhausted,
}

async fn exhaust_supervision_by_failure(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    agent_id: &AgentId,
    expected_activity_counter: u64,
    idle_since: Instant,
    attempts_started: u8,
    expected_settings: SupervisorSettingsSignal,
) {
    let Some(observation) = host.activity_summary_observation(agent_id).await else {
        entries.remove(agent_id);
        return;
    };
    let settings_rx = host.supervisor_settings_receiver().await;
    let outcome = observation
        .handle
        .append_supervisor_failure_warning_if_inactive(
            expected_activity_counter,
            attempts_started,
            expected_settings,
            settings_rx,
        )
        .await;
    match outcome {
        AppendSupervisorWarningOutcome::Appended
        | AppendSupervisorWarningOutcome::AlreadyAppended => {
            if let Some(entry) = entries.get_mut(agent_id)
                && entry.last_activity_counter == expected_activity_counter
            {
                entry.phase = SupervisorPhase::Dormant { idle_since };
                tracing::warn!(
                    agent_id = %agent_id,
                    attempts_started,
                    maximum_attempts = expected_settings.settings.retry_attempts.saturating_add(1),
                    "agent supervision verdict attempts exhausted; leaving the agent idle"
                );
            } else {
                observe_supervised_agents(host, entries).await;
            }
        }
        AppendSupervisorWarningOutcome::ActivityChanged
        | AppendSupervisorWarningOutcome::Ineligible => {
            observe_supervised_agents(host, entries).await;
        }
        AppendSupervisorWarningOutcome::SettingsChanged { live } => {
            Box::pin(apply_supervisor_settings_change(
                host,
                entries,
                expected_settings,
                live,
            ))
            .await;
        }
        AppendSupervisorWarningOutcome::Closed => {
            entries.remove(agent_id);
            tracing::info!(
                agent_id = %agent_id,
                attempts_started,
                "dropping supervisor failure warning because the affected agent closed"
            );
        }
    }
}

fn schedule_supervision_retry_at(
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    agent_id: &AgentId,
    idle_since: Instant,
    baseline: SupervisionBaseline,
    attempts_started: u8,
    settings: protocol::SupervisorSettings,
    reason: SupervisionRetryReason,
    message: Option<String>,
    scheduled_at: Instant,
) -> SupervisionRetrySchedule {
    let maximum_attempts = settings.retry_attempts.saturating_add(1);
    if attempts_started >= maximum_attempts {
        entries.get_mut(agent_id).expect("entry exists").phase = match reason {
            SupervisionRetryReason::Failure(last_failure_kind) => {
                let retry_due_at = SUPERVISION_RETRY_DELAYS
                    .get(usize::from(attempts_started.saturating_sub(1)))
                    .and_then(|delay| scheduled_at.checked_add(*delay));
                SupervisorPhase::FailureExhausted {
                    idle_since,
                    baseline,
                    attempts_started,
                    retry_due_at,
                    last_failure_kind,
                }
            }
            SupervisionRetryReason::SettingsChanged => SupervisorPhase::Dormant { idle_since },
        };
        return SupervisionRetrySchedule::Exhausted;
    }
    let delay = SUPERVISION_RETRY_DELAYS[usize::from(attempts_started.saturating_sub(1))];
    let due_at = scheduled_at.checked_add(delay).unwrap_or(scheduled_at);
    entries.get_mut(agent_id).expect("entry exists").phase = SupervisorPhase::RetryPending {
        idle_since,
        baseline,
        attempts_started,
        due_at,
        last_failure_kind: reason.clone(),
        verdict_settings: VerdictSettingsFingerprint::from(settings),
    };
    tracing::info!(
        agent_id = %agent_id,
        attempts_started,
        maximum_attempts,
        failure_kind = ?reason,
        error = ?message,
        backoff_seconds = delay.as_secs(),
        next_due = ?due_at,
        "agent supervision verdict failed; scheduling delayed attempt"
    );
    SupervisionRetrySchedule::Pending
}

async fn accept_supervision_verdict_result(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    settings: SupervisorSettingsSignal,
    result: SupervisorVerdictTaskResult,
    result_dequeued_at: Instant,
) {
    let live_settings = host.supervisor_settings_signal().await;
    wait_for_supervisor_verdict_post_sample_test_gate(&result.agent_id).await;
    let Some(entry) = entries.get(&result.agent_id) else {
        return;
    };
    let SupervisorPhase::VerdictInFlight {
        idle_since,
        baseline,
        attempts_started,
        verdict_settings,
    } = &entry.phase
    else {
        return;
    };
    if entry.last_activity_counter != result.activity_counter
        || baseline != &result.baseline
        || *attempts_started != result.attempts_started
        || *verdict_settings != result.verdict_settings
    {
        tracing::debug!(
            agent_id = %result.agent_id,
            "dropping stale supervision verdict: generation changed"
        );
        return;
    }
    let idle_since = *idle_since;
    if !live_settings.settings.enabled {
        entries.remove(&result.agent_id);
        tracing::debug!(
            agent_id = %result.agent_id,
            "dropping supervision verdict after the supervisor was disabled"
        );
        return;
    }
    if VerdictSettingsFingerprint::from(settings.settings) != result.verdict_settings
        || VerdictSettingsFingerprint::from(live_settings.settings) != result.verdict_settings
    {
        if schedule_supervision_retry_at(
            entries,
            &result.agent_id,
            idle_since,
            result.baseline.clone(),
            result.attempts_started,
            live_settings.settings,
            SupervisionRetryReason::SettingsChanged,
            None,
            result_dequeued_at,
        ) == SupervisionRetrySchedule::Exhausted
        {
            entries
                .get_mut(&result.agent_id)
                .expect("entry exists")
                .phase = SupervisorPhase::Dormant { idle_since };
        }
        return;
    }
    let Some(observation) = host.activity_summary_observation(&result.agent_id).await else {
        entries.remove(&result.agent_id);
        return;
    };
    if observation.status.activity_counter != result.activity_counter
        || observation.status.terminated
        || observation.status.is_active()
        || observation.status.is_plan_approval_pending()
    {
        observe_supervised_agents(host, entries).await;
        return;
    }
    if observation.start.session_id != result.baseline.session_id {
        entries
            .get_mut(&result.agent_id)
            .expect("entry exists")
            .phase = SupervisorPhase::Dormant { idle_since };
        return;
    }
    let Some(context) = observation.handle.read_supervision_context().await else {
        entries
            .get_mut(&result.agent_id)
            .expect("entry exists")
            .phase = SupervisorPhase::Dormant { idle_since };
        tracing::warn!(
            agent_id = %result.agent_id,
            "dropping supervision verdict: live context reader is unavailable"
        );
        return;
    };
    if context.last_user_message.as_deref() != Some(result.baseline.last_user_message.as_str())
        || context.kicks_since_user_message != result.baseline.kicks_since_user_message
        || context.cancelled_since_user_message
        || !supervision_session_allows_action(host, &observation, &context).await
    {
        tracing::debug!(
            agent_id = %result.agent_id,
            "dropping stale supervision verdict: conversation moved on"
        );
        entries
            .get_mut(&result.agent_id)
            .expect("entry exists")
            .phase = SupervisorPhase::Dormant { idle_since };
        return;
    }

    let final_settings = host.supervisor_settings_signal().await;
    if !final_settings.settings.enabled {
        entries.remove(&result.agent_id);
        tracing::debug!(
            agent_id = %result.agent_id,
            "dropping supervision verdict after the supervisor was disabled"
        );
        return;
    }
    if VerdictSettingsFingerprint::from(final_settings.settings) != result.verdict_settings {
        if schedule_supervision_retry_at(
            entries,
            &result.agent_id,
            idle_since,
            result.baseline.clone(),
            result.attempts_started,
            final_settings.settings,
            SupervisionRetryReason::SettingsChanged,
            None,
            result_dequeued_at,
        ) == SupervisionRetrySchedule::Exhausted
        {
            entries
                .get_mut(&result.agent_id)
                .expect("entry exists")
                .phase = SupervisorPhase::Dormant { idle_since };
        }
        return;
    }

    let verdict = match result.result {
        Ok(verdict) => verdict,
        Err(error) => {
            let failure_kind = error.kind;
            let failure_message = error.message;
            let schedule = schedule_supervision_retry_at(
                entries,
                &result.agent_id,
                idle_since,
                result.baseline,
                result.attempts_started,
                final_settings.settings,
                SupervisionRetryReason::Failure(failure_kind),
                Some(failure_message.clone()),
                result_dequeued_at,
            );
            if schedule == SupervisionRetrySchedule::Exhausted {
                tracing::warn!(
                    agent_id = %result.agent_id,
                    attempts_started = result.attempts_started,
                    maximum_attempts = final_settings.settings.retry_attempts.saturating_add(1),
                    failure_kind = ?failure_kind,
                    error = %failure_message,
                    "agent supervision verdict failure exhausted its retry budget"
                );
                exhaust_supervision_by_failure(
                    host,
                    entries,
                    &result.agent_id,
                    result.activity_counter,
                    idle_since,
                    result.attempts_started,
                    final_settings,
                )
                .await;
            }
            return;
        }
    };
    tracing::info!(
        agent_id = %result.agent_id,
        attempts_started = result.attempts_started,
        verdict = ?verdict,
        "agent supervision verdict attempt succeeded"
    );
    match verdict {
        crate::agent::supervisor::SupervisionVerdict::Continue { message } => {
            tracing::info!(
                agent_id = %result.agent_id,
                kick = result.baseline.kicks_since_user_message + 1,
                "supervisor kicking idle agent to continue"
            );
            entries
                .get_mut(&result.agent_id)
                .expect("entry exists")
                .phase = SupervisorPhase::Active;
            if let Some(status_handle) = host.agent_status_handle(&result.agent_id).await {
                status_handle.update(mark_supervisor_kick_pending).await;
            }
            let sent = observation
                .handle
                .send_input(AgentInput::SendMessage(SendMessagePayload {
                    message: format!("{SUPERVISOR_MESSAGE_PREFIX}{message}"),
                    images: None,
                    origin: Some(MessageOrigin::Supervisor),
                    tool_response: None,
                }))
                .await;
            if !sent {
                tracing::warn!(
                    agent_id = %result.agent_id,
                    "supervisor kick could not be delivered: agent backend is closed"
                );
            }
        }
        crate::agent::supervisor::SupervisionVerdict::AwaitingUser => {
            tracing::info!(
                agent_id = %result.agent_id,
                "supervisor classified the idle turn as awaiting user input"
            );
            entries
                .get_mut(&result.agent_id)
                .expect("entry exists")
                .phase = SupervisorPhase::AwaitingUser { idle_since };
        }
        crate::agent::supervisor::SupervisionVerdict::Done => {
            tracing::info!(
                agent_id = %result.agent_id,
                "supervisor confirmed the requested work is truly complete"
            );
            entries
                .get_mut(&result.agent_id)
                .expect("entry exists")
                .phase = SupervisorPhase::DoneAuthorized {
                idle_since,
                baseline: result.baseline,
                last_gate_evaluation_epoch: None,
            };
        }
    }
}

async fn supervision_session_allows_action(
    host: &HostHandle,
    observation: &ActivitySummaryObservation,
    context: &crate::agent::supervisor::SupervisionContextSnapshot,
) -> bool {
    let Some(session_id) = observation.start.session_id.as_ref() else {
        return true;
    };
    let session_store = { host.state.lock().await.session_store.clone() };
    let record = session_store.lock().await.get(session_id);
    let Some(record) = record else {
        return false;
    };
    if record.compacted_to_session_id.is_some() {
        return false;
    }
    !(record.compacted_from_session_id.is_some() && context.user_message_count <= 1)
}

async fn launch_supervisor_auto_compaction(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    agent_id: AgentId,
    settings: SupervisorSettingsSignal,
    compaction_tx: &mpsc::UnboundedSender<SupervisorCompactionTaskEvent>,
) {
    let Some(entry) = entries.get(&agent_id) else {
        return;
    };
    let SupervisorPhase::DoneAuthorized {
        idle_since,
        baseline,
        ..
    } = &entry.phase
    else {
        return;
    };
    let idle_since = *idle_since;
    let baseline = baseline.clone();
    let activity_counter = entry.last_activity_counter;

    if !settings.settings.enabled || !settings.settings.auto_compact_on_success {
        return;
    }
    let Some(observation) = host.activity_summary_observation(&agent_id).await else {
        entries.remove(&agent_id);
        return;
    };
    if observation.status.activity_counter != activity_counter
        || observation.status.terminated
        || observation.status.is_active()
        || observation.status.is_plan_approval_pending()
    {
        observe_supervised_agents(host, entries).await;
        return;
    }
    if observation.start.session_id != baseline.session_id {
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        tracing::info!(
            agent_id = %agent_id,
            "skipping supervisor auto-compaction: session identity changed"
        );
        return;
    }
    let Some(context) = observation.handle.read_supervision_context().await else {
        mark_supervisor_gate_evaluated(entries, &agent_id, settings.epoch);
        tracing::info!(
            agent_id = %agent_id,
            "skipping supervisor auto-compaction: supervision context is unavailable"
        );
        return;
    };
    if context.last_user_message.as_deref() != Some(baseline.last_user_message.as_str())
        || context.kicks_since_user_message != baseline.kicks_since_user_message
        || context.cancelled_since_user_message
        || !supervision_session_allows_action(host, &observation, &context).await
    {
        entries.get_mut(&agent_id).expect("entry exists").phase =
            SupervisorPhase::Dormant { idle_since };
        tracing::info!(
            agent_id = %agent_id,
            "skipping supervisor auto-compaction: conversation or session changed"
        );
        return;
    }

    let threshold = settings.settings.auto_compact_min_context_tokens;
    let current_context = context.current_context_input_tokens;
    if !supervisor_auto_compaction_eligible(current_context, threshold) {
        mark_supervisor_gate_evaluated(entries, &agent_id, settings.epoch);
        match current_context {
            Some(current) => tracing::info!(
                agent_id = %agent_id,
                current_context_input_tokens = current,
                auto_compact_min_context_tokens = threshold,
                "skipping supervisor auto-compaction: current context is at or below the configured minimum"
            ),
            None => tracing::info!(
                agent_id = %agent_id,
                auto_compact_min_context_tokens = threshold,
                "skipping supervisor auto-compaction: current context usage is unavailable"
            ),
        }
        return;
    }

    entries.get_mut(&agent_id).expect("entry exists").phase =
        SupervisorPhase::CompactionPending { idle_since };
    let host = host.clone();
    let tx = compaction_tx.clone();
    tokio::spawn(async move {
        supervisor_auto_compact(host, agent_id, activity_counter, settings.epoch, tx).await;
    });
}

fn mark_supervisor_gate_evaluated(
    entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
    agent_id: &AgentId,
    settings_epoch: u64,
) {
    let Some(entry) = entries.get_mut(agent_id) else {
        return;
    };
    if let SupervisorPhase::DoneAuthorized {
        last_gate_evaluation_epoch,
        ..
    } = &mut entry.phase
    {
        *last_gate_evaluation_epoch = Some(settings_epoch);
    }
}

fn supervisor_auto_compaction_eligible(
    current_context_input_tokens: Option<u64>,
    minimum_context_tokens: u64,
) -> bool {
    current_context_input_tokens.is_some_and(|current| current > minimum_context_tokens)
}

async fn supervisor_auto_compact(
    host: HostHandle,
    agent_id: AgentId,
    expected_activity_counter: u64,
    expected_supervisor_settings_epoch: u64,
    task_tx: mpsc::UnboundedSender<SupervisorCompactionTaskEvent>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stream = Stream::new(
        StreamPath(format!("/agent/{}/supervisor-compact", agent_id)),
        tx,
    );
    let accepted = match host
        .compact_agent_if_inactive_in_background(
            agent_id.clone(),
            expected_activity_counter,
            expected_supervisor_settings_epoch,
            AgentCompactPayload {
                summary_prompt: None,
                max_summary_bytes: None,
            },
            stream,
        )
        .await
    {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::warn!(
                agent_id = %agent_id,
                error = %error,
                "supervisor auto-compaction could not start"
            );
            false
        }
    };
    let _ = task_tx.send(SupervisorCompactionTaskEvent::Started {
        agent_id: agent_id.clone(),
        activity_counter: expected_activity_counter,
        accepted,
    });
    if !accepted {
        return;
    }

    let observe = async {
        while let Some(envelope) = rx.recv().await {
            if envelope.kind != FrameKind::AgentCompactNotify {
                continue;
            }
            let Ok(payload) = envelope.parse_payload::<AgentCompactNotifyPayload>() else {
                continue;
            };
            match payload.status {
                AgentCompactStatus::Completed => {
                    tracing::info!(
                        agent_id = %agent_id,
                        new_agent_id = ?payload.new_agent_id,
                        "supervisor auto-compaction completed"
                    );
                    return;
                }
                AgentCompactStatus::Failed => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        message = ?payload.message,
                        "supervisor auto-compaction failed"
                    );
                    return;
                }
                _ => {}
            }
        }
    };
    if tokio::time::timeout(SUPERVISION_COMPACTION_OBSERVE_TIMEOUT, observe)
        .await
        .is_err()
    {
        tracing::warn!(
            agent_id = %agent_id,
            "supervisor auto-compaction did not reach a terminal notify in time"
        );
    }
    let _ = task_tx.send(SupervisorCompactionTaskEvent::Finished {
        agent_id,
        activity_counter: expected_activity_counter,
    });
}

async fn observe_activity_summary_agents(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, ActivitySummarySchedulerEntry>,
) {
    let observations = host.activity_summary_observations().await;
    let live_agent_ids = observations
        .iter()
        .map(|observation| observation.agent_id.clone())
        .collect::<HashSet<_>>();
    entries.retain(|agent_id, _| live_agent_ids.contains(agent_id));

    for observation in observations {
        let entry = entries.entry(observation.agent_id.clone()).or_default();
        if entry.last_activity_counter == observation.status.activity_counter
            && entry.latest_observed_through_seq.is_some()
        {
            continue;
        }
        entry.last_activity_counter = observation.status.activity_counter;

        let Some(history) = observation
            .handle
            .read_activity_history(
                None,
                ACTIVITY_SUMMARY_HISTORY_EVENTS,
                ACTIVITY_SUMMARY_HISTORY_BYTES,
            )
            .await
        else {
            host.set_agent_activity_summary_error(
                observation.agent_id.clone(),
                "agent activity history reader closed".to_owned(),
            )
            .await;
            continue;
        };

        if history.event_count == 0 || history.through_seq.is_none() {
            host.set_agent_activity_summary_empty(observation.agent_id.clone())
                .await;
            continue;
        }

        let first_observed_history = entry.latest_observed_through_seq.is_none();
        if entry.latest_observed_through_seq != history.through_seq {
            entry.latest_observed_through_seq = history.through_seq;
            host.mark_agent_activity_summary_stale_if_fresh(observation.agent_id.clone())
                .await;
        }

        if history.through_seq == entry.last_summarized_through_seq {
            continue;
        }

        let now = Instant::now();
        let is_active = observation.status.is_active();
        let final_refresh = !is_active && (entry.was_active || first_observed_history);
        if !is_active && !final_refresh {
            continue;
        }
        if is_active {
            entry.was_active = true;
        }
        if entry.first_meaningful_at.is_none() {
            entry.first_meaningful_at = Some(now);
        }
        let mut due = if final_refresh {
            entry.queued_final_refresh = true;
            now + ACTIVITY_SUMMARY_DEBOUNCE
        } else {
            let initial_due = entry
                .first_meaningful_at
                .expect("first meaningful timestamp must be set")
                + ACTIVITY_SUMMARY_INITIAL_DELAY;
            std::cmp::max(now + ACTIVITY_SUMMARY_DEBOUNCE, initial_due)
        };
        if !final_refresh && let Some(last_call_at) = entry.last_call_at {
            due = std::cmp::max(due, last_call_at + ACTIVITY_SUMMARY_MAX_FREQUENCY);
        }
        if let Some(backoff_until) = entry.backoff_until {
            due = std::cmp::max(due, backoff_until);
        }
        entry.pending_due = Some(due);
    }
}

async fn start_due_activity_summary_calls(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, ActivitySummarySchedulerEntry>,
    settings: ActivitySummarySettingsSignal,
    semaphore: Arc<Semaphore>,
    task_event_tx: mpsc::UnboundedSender<ActivitySummaryTaskEvent>,
) {
    let now = Instant::now();
    let due_agent_ids = entries
        .iter()
        .filter_map(|(agent_id, entry)| {
            (!entry.in_flight && entry.pending_due.is_some_and(|due| due <= now))
                .then_some(agent_id.clone())
        })
        .collect::<Vec<_>>();

    for agent_id in due_agent_ids {
        let Some(entry) = entries.get_mut(&agent_id) else {
            continue;
        };
        let Some(context) = host.activity_summary_observation(&agent_id).await else {
            entries.remove(&agent_id);
            continue;
        };
        let Some(history) = context
            .handle
            .read_activity_history(
                None,
                ACTIVITY_SUMMARY_HISTORY_EVENTS,
                ACTIVITY_SUMMARY_HISTORY_BYTES,
            )
            .await
        else {
            host.set_agent_activity_summary_error(
                agent_id.clone(),
                "agent activity history reader closed".to_owned(),
            )
            .await;
            entry.pending_due = None;
            continue;
        };
        if history.event_count == 0 || history.through_seq.is_none() {
            host.set_agent_activity_summary_empty(agent_id.clone())
                .await;
            entry.pending_due = None;
            continue;
        }
        if history.through_seq == entry.last_summarized_through_seq {
            entry.pending_due = None;
            continue;
        }

        let previous_summary = host.agent_activity_summary_for_context(&agent_id).await;
        let previous_text = previous_summary
            .as_ref()
            .map(|summary| summary.text.clone());
        let transient_agent_id = AgentId(Uuid::new_v4().to_string());
        debug_assert!(
            !host.is_agent_registered(&transient_agent_id).await,
            "activity summary transient agent id was registered before spawn"
        );

        entry.in_flight = true;
        entry.pending_due = None;
        entry.queued_final_refresh = false;
        entry.last_call_at = Some(now);

        let request = GenerateAgentActivitySummaryRequest {
            summary_agent_id: transient_agent_id.clone(),
            backend_kind: context.start.backend_kind,
            workspace_roots: context.start.workspace_roots.clone(),
            rendered_history: history.rendered,
            previous_summary: previous_text,
            source_from_seq: history.from_seq,
            source_through_seq: history.through_seq,
            use_mock_backend: host.use_mock_backend().await,
            capacity_tx: host.state.lock().await.capacity_tx.clone(),
        };
        let task_event_tx = task_event_tx.clone();
        let host_for_assert = host.clone();
        let semaphore = Arc::clone(&semaphore);
        tokio::spawn(async move {
            let permit = match semaphore.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = task_event_tx.send(ActivitySummaryTaskEvent::Finished(
                        ActivitySummaryTaskResult {
                            agent_id,
                            epoch: settings.epoch,
                            transient_agent_id,
                            source_through_seq: request.source_through_seq,
                            result: Err("activity summary concurrency limiter closed".to_owned()),
                        },
                    ));
                    return;
                }
            };
            debug_assert!(
                !host_for_assert
                    .is_agent_registered(&transient_agent_id)
                    .await,
                "activity summary transient agent id was registered before direct backend spawn"
            );
            let source_through_seq = request.source_through_seq;
            let _ = task_event_tx.send(ActivitySummaryTaskEvent::Started(
                ActivitySummaryTaskStarted {
                    agent_id: agent_id.clone(),
                    epoch: settings.epoch,
                    requested_at_ms: crate::agent::now_ms(),
                    previous_summary,
                },
            ));
            let result = await_activity_summary_generation(
                generate_agent_activity_summary(request),
                ACTIVITY_SUMMARY_GENERATION_TIMEOUT,
            )
            .await;
            debug_assert!(
                !host_for_assert
                    .is_agent_registered(&transient_agent_id)
                    .await,
                "activity summary transient agent id was registered after direct backend spawn"
            );
            drop(permit);
            let _ = task_event_tx.send(ActivitySummaryTaskEvent::Finished(
                ActivitySummaryTaskResult {
                    agent_id,
                    epoch: settings.epoch,
                    transient_agent_id,
                    source_through_seq,
                    result,
                },
            ));
        });
    }
}

async fn begin_activity_summary_call(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, ActivitySummarySchedulerEntry>,
    started: ActivitySummaryTaskStarted,
    settings: ActivitySummarySettingsSignal,
) {
    if !settings.enabled || settings.epoch != started.epoch {
        return;
    }
    if !entries.contains_key(&started.agent_id) {
        return;
    }
    if !host.is_agent_registered(&started.agent_id).await {
        entries.remove(&started.agent_id);
        return;
    }

    host.set_agent_activity_summary_state(
        started.agent_id,
        AgentActivitySummaryState::Pending {
            requested_at_ms: started.requested_at_ms,
            previous: started.previous_summary,
        },
    )
    .await;
}

async fn await_activity_summary_generation<F>(
    generation: F,
    timeout: Duration,
) -> Result<AgentActivitySummary, String>
where
    F: Future<Output = Result<AgentActivitySummary, String>>,
{
    match tokio::time::timeout(timeout, generation).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "activity summary generation timed out after {}",
            generation_timeout_label(timeout)
        )),
    }
}

async fn await_agent_name_generation<F>(generation: F, timeout: Duration) -> Result<String, String>
where
    F: Future<Output = Result<String, String>>,
{
    match tokio::time::timeout(timeout, generation).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "agent name generation timed out after {}",
            generation_timeout_label(timeout)
        )),
    }
}

fn generation_timeout_label(timeout: Duration) -> String {
    if timeout.subsec_nanos() == 0 && timeout.as_secs() == 1 {
        return "1 second".to_owned();
    }
    if timeout.subsec_nanos() == 0 {
        return format!("{} seconds", timeout.as_secs());
    }
    format!("{} ms", timeout.as_millis())
}

async fn finish_activity_summary_call(
    host: &HostHandle,
    entries: &mut HashMap<AgentId, ActivitySummarySchedulerEntry>,
    result: ActivitySummaryTaskResult,
    settings: ActivitySummarySettingsSignal,
) {
    debug_assert!(
        !host.is_agent_registered(&result.transient_agent_id).await,
        "activity summary transient agent id was registered when result was handled"
    );
    let Some(entry) = entries.get_mut(&result.agent_id) else {
        return;
    };
    entry.in_flight = false;

    if !settings.enabled || settings.epoch != result.epoch {
        return;
    }
    if !host.is_agent_registered(&result.agent_id).await {
        entries.remove(&result.agent_id);
        return;
    }

    match result.result {
        Ok(summary) => {
            entry.last_summarized_through_seq = result.source_through_seq;
            entry.backoff_until = None;
            host.set_agent_activity_summary_state(
                result.agent_id,
                AgentActivitySummaryState::Fresh { summary },
            )
            .await;
        }
        Err(message) => {
            entry.backoff_until = Some(Instant::now() + ACTIVITY_SUMMARY_FAILURE_BACKOFF);
            host.set_agent_activity_summary_error(result.agent_id, message)
                .await;
        }
    }
}

fn spawn_host_capacity_task(host: HostHandle, mut rx: HostCapacityRx) {
    let worker = async move {
        while let Some(update) = rx.recv().await {
            match update {
                HostCapacityUpdate::Report {
                    backend_kind,
                    state,
                } => {
                    host.record_backend_capacity(backend_kind, state).await;
                }
                #[cfg(feature = "test-support")]
                HostCapacityUpdate::Barrier(reply) => {
                    let _ = reply.send(());
                }
            }
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-host-capacity".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build host capacity runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn host capacity task");
}

fn spawn_host_spawn_operation_task(
    host: WeakHostHandle,
    mut rx: mpsc::Receiver<SpawnOperation>,
    cancel: CancellationToken,
    #[cfg(feature = "test-support")] completion_test_gate: Arc<
        StdMutex<Option<Arc<SpawnOperationTestGateInner>>>,
    >,
    #[cfg(feature = "test-support")] start_test_gate: Arc<
        StdMutex<Option<Arc<SpawnOperationTestGateInner>>>,
    >,
    #[cfg(feature = "test-support")] drain_test_gate: Arc<
        StdMutex<Option<Arc<SpawnOperationTestGateInner>>>,
    >,
    #[cfg(feature = "test-support")] publication_test_gate: Arc<
        StdMutex<Option<Arc<SpawnOperationTestGateInner>>>,
    >,
) -> SpawnOperationWorker {
    let worker = async move {
        let mut operations = JoinSet::new();
        let mut active = HashMap::new();
        loop {
            if operations.len() >= MAX_CONCURRENT_SPAWN_OPERATIONS {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    result = operations.join_next_with_id() => {
                        if let Some(result) = result {
                            finish_spawn_operation_result(result, &mut active, false);
                        }
                    }
                }
                continue;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                result = operations.join_next_with_id(), if !operations.is_empty() => {
                    if let Some(result) = result {
                        finish_spawn_operation_result(result, &mut active, false);
                    }
                }
                operation = rx.recv() => {
                    let Some(operation) = operation else {
                        break;
                    };
                    let Some(host) = host.upgrade() else {
                        break;
                    };
                    let terminal = SpawnOperationTerminal {
                        request_stream: operation.request_stream,
                        output_stream: operation.output_stream,
                    };
                    let outcome = Arc::new(StdMutex::new(None));
                    let terminal_claim = SpawnOperationTerminalClaim {
                        outcome: Arc::clone(&outcome),
                        #[cfg(feature = "test-support")]
                        publication_test_gate: Arc::clone(&publication_test_gate),
                    };
                    #[cfg(feature = "test-support")]
                    let completion_test_gate = Arc::clone(&completion_test_gate);
                    #[cfg(feature = "test-support")]
                    let start_test_gate = Arc::clone(&start_test_gate);
                    let abort_handle = operations.spawn(async move {
                        let result = AssertUnwindSafe(async {
                            #[cfg(feature = "test-support")]
                            wait_for_spawn_operation_test_gate(&start_test_gate).await;
                            host.spawn_agent_for_operation(
                                operation.payload,
                                terminal_claim.clone(),
                            )
                            .await
                        })
                        .catch_unwind()
                        .await;
                        terminal_claim.claim_resolved_result(result);
                        #[cfg(feature = "test-support")]
                        wait_for_spawn_operation_test_gate(&completion_test_gate).await;
                    });
                    active.insert(abort_handle.id(), ActiveSpawnOperation { terminal, outcome });
                }
            }
        }
        rx.close();
        operations.abort_all();
        while let Some(result) = operations.join_next_with_id().await {
            finish_spawn_operation_result(result, &mut active, true);
        }
        while let Some(operation) = rx.recv().await {
            emit_spawn_operation_shutdown(SpawnOperationTerminal {
                request_stream: operation.request_stream,
                output_stream: operation.output_stream,
            });
        }
        #[cfg(feature = "test-support")]
        wait_for_spawn_operation_test_gate(&drain_test_gate).await;
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return SpawnOperationWorker::Tokio(handle.spawn(worker));
    }

    SpawnOperationWorker::Thread(
        std::thread::Builder::new()
            .name("tyde-host-spawn-operations".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build host spawn operation runtime");
                runtime.block_on(worker);
            })
            .expect("failed to spawn host spawn operation worker thread"),
    )
}

fn finish_spawn_operation_result(
    result: Result<(tokio::task::Id, ()), tokio::task::JoinError>,
    active: &mut HashMap<tokio::task::Id, ActiveSpawnOperation>,
    shutting_down: bool,
) {
    let (task_id, join_error) = match result {
        Ok((task_id, ())) => (task_id, None),
        Err(error) => (error.id(), Some(error)),
    };
    let Some(operation) = active.remove(&task_id) else {
        return;
    };
    let outcome = operation
        .outcome
        .lock()
        .expect("spawn operation outcome mutex poisoned")
        .take();
    match outcome {
        Some(SpawnOperationOutcome::Success) => {}
        Some(SpawnOperationOutcome::Error(error)) => {
            emit_spawn_operation_error(&operation.terminal, &error);
        }
        Some(SpawnOperationOutcome::Panicked) => {
            emit_spawn_operation_abnormal(operation.terminal, "spawn operation panicked");
        }
        None if shutting_down
            && join_error
                .as_ref()
                .is_some_and(|error| error.is_cancelled()) =>
        {
            emit_spawn_operation_shutdown(operation.terminal);
        }
        None => {
            let message = join_error.map_or_else(
                || "spawn operation completed without an outcome".to_owned(),
                |error| format!("spawn operation terminated unexpectedly: {error}"),
            );
            emit_spawn_operation_abnormal(operation.terminal, &message);
        }
    }
}

fn emit_spawn_operation_error(terminal: &SpawnOperationTerminal, error: &AppError) {
    crate::connection::emit_command_error(
        &terminal.output_stream,
        terminal.request_stream.clone(),
        FrameKind::SpawnAgent,
        None,
        error,
    );
}

fn emit_spawn_operation_abnormal(terminal: SpawnOperationTerminal, message: &str) {
    emit_spawn_operation_error(
        &terminal,
        &AppError::internal_message(
            "spawn_agent",
            message.to_owned(),
            anyhow!(message.to_owned()),
        ),
    );
}

fn emit_spawn_operation_shutdown(terminal: SpawnOperationTerminal) {
    emit_spawn_operation_abnormal(terminal, "host shut down before spawn operation completed");
}

fn spawn_host_sub_agent_task(host: HostHandle, mut rx: HostSubAgentSpawnRx) {
    let worker = async move {
        while let Some(request) = rx.recv().await {
            let result = host.spawn_backend_native_subagent(&request).await;
            let _ = request.reply.send(result);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-host-subagents".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build host sub-agent runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn host sub-agent worker thread");
}

fn spawn_host_review_delivery_task(
    host: HostHandle,
    mut rx: mpsc::Receiver<ReviewDeliveryRequest>,
) {
    let worker = async move {
        while let Some(request) = rx.recv().await {
            let review_id = request.review_id.clone();
            let project_id = request.project_id.clone();
            let message_len = request.payload.message.len();
            let images_count = request.payload.images.as_ref().map_or(0, Vec::len);
            tracing::debug!(
                review_id = %review_id,
                project_id = %project_id,
                message_len,
                images_count,
                "review delivery worker received request"
            );
            let outcome = host
                .deliver_review_payload(
                    request.review_id,
                    request.project_id,
                    request.target,
                    request.payload,
                )
                .await;
            tracing::debug!(
                review_id = %review_id,
                project_id = %project_id,
                outcome = outcome.label(),
                "review delivery worker completed request"
            );
            let _ = request.reply.send(outcome);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-delivery".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review delivery runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn review delivery worker thread");
}

fn spawn_host_review_ai_task(host: HostHandle, mut rx: mpsc::Receiver<ReviewAiSpawnRequest>) {
    let worker = async move {
        while let Some(request) = rx.recv().await {
            let (reply, result) = host.spawn_ai_reviewer(request).await;
            let _ = reply.send(result);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-ai".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review AI runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn review AI worker thread");
}

fn spawn_host_review_project_update_task(
    host: HostHandle,
    mut rx: mpsc::UnboundedReceiver<ProjectId>,
) {
    let worker = async move {
        while let Some(project_id) = rx.recv().await {
            host.emit_review_list_changed(project_id).await;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-review-projects".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build review project update runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn review project update worker thread");
}

fn spawn_host_workflow_catalog_task(
    host: HostHandle,
    mut rx: mpsc::Receiver<WorkflowCatalogSignal>,
) {
    let worker = async move {
        while let Some(signal) = rx.recv().await {
            let result = match signal {
                WorkflowCatalogSignal::Rescan { reason } => {
                    let reason: &'static str = match reason.as_str() {
                        "workflow_fs_watch" => "workflow_fs_watch",
                        "workflow_watch_target_created" => "workflow_watch_target_created",
                        _ => "workflow_catalog_signal",
                    };
                    host.reload_workflows_and_notify(reason).await
                }
                WorkflowCatalogSignal::WatcherError { message } => {
                    let diagnostic = WorkflowDiagnostic {
                        workflow_id: None,
                        source: None,
                        severity: WorkflowDiagnosticSeverity::Error,
                        message,
                    };
                    host.reload_workflows_and_notify_inner("workflow_fs_watch", Some(diagnostic))
                        .await
                }
            };
            if let Err(error) = result {
                tracing::warn!(
                    error = %error,
                    "failed to reload workflows after workflow catalog signal"
                );
            }
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    std::thread::Builder::new()
        .name("tyde-workflow-catalog".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build workflow catalog runtime");
            runtime.block_on(worker);
        })
        .expect("failed to spawn workflow catalog worker thread");
}

pub(crate) fn startup_mcp_servers_for_settings(
    settings: &protocol::HostSettings,
    workspace_roots: &[String],
    debug_mcp: &DebugMcpHandle,
    agent_control_mcp: &AgentControlMcpHandle,
    config_mcp: &ConfigMcpHandle,
    custom_agent_id: Option<&protocol::CustomAgentId>,
) -> Vec<StartupMcpServer> {
    let mut servers = Vec::new();

    // The builtin Help agent gets the host-configuration tools; no other
    // agent does.
    if custom_agent_id.is_some_and(|id| id.0 == crate::store::custom_agents::HELP_CUSTOM_AGENT_ID)
        && !config_mcp.url.is_empty()
    {
        servers.push(StartupMcpServer {
            name: "tyde-config".to_string(),
            transport: StartupMcpTransport::Http {
                url: config_mcp.url.clone(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        });
    }

    if settings.tyde_debug_mcp_enabled {
        let mut headers = HashMap::new();
        if let Some(repo_root) = workspace_roots
            .first()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            headers.insert(
                crate::debug_mcp::DEBUG_REPO_ROOT_HEADER.to_string(),
                repo_root.to_string(),
            );
        }
        let url = workspace_roots
            .first()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|repo_root| debug_mcp_url_for_repo_root(&debug_mcp.url, repo_root))
            .unwrap_or_else(|| debug_mcp.url.clone());
        servers.push(StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var: None,
            },
        });
    }

    if settings.tyde_agent_control_mcp_enabled && !agent_control_mcp.url.is_empty() {
        servers.push(StartupMcpServer {
            name: crate::agent_control_mcp::AGENT_CONTROL_MCP_SERVER_NAME.to_string(),
            transport: StartupMcpTransport::Http {
                url: agent_control_mcp.url.clone(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        });
        servers.push(StartupMcpServer {
            name: crate::agent_control_mcp::AGENT_CONTROL_AWAIT_MCP_SERVER_NAME.to_string(),
            transport: StartupMcpTransport::Http {
                url: agent_control_mcp.await_url.clone(),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
        });
    }

    servers
}

fn prepend_manager_roster(team: &protocol::Team, members: &[TeamMember], prompt: String) -> String {
    let mut block = String::new();
    block.push_str(
        "You are the manager for this Tyde agent team. Current roster:
",
    );
    block.push_str(&format!(
        "Team: {} ({})
",
        team.name, team.id
    ));
    for member in members {
        if member.role != TeamMemberRole::Report {
            continue;
        }
        block.push_str(
            "
Report:
",
        );
        block.push_str(&format!(
            "- member_id: {}
",
            member.id
        ));
        block.push_str(&format!(
            "- name: {}
",
            member.name
        ));
        block.push_str(&format!(
            "- description: {}
",
            member.description
        ));
        if let Some(profile) = member.profile.as_ref() {
            if let Some(role_preset_id) = profile.role_preset_id.as_ref() {
                block.push_str(&format!("- role_preset_id: {}\n", role_preset_id));
            }
            if let Some(personality_preset_id) = profile.personality_preset_id.as_ref() {
                block.push_str(&format!(
                    "- personality_preset_id: {}\n",
                    personality_preset_id
                ));
            }
            if !profile.personality_traits.is_empty() {
                block.push_str(&format!(
                    "- personality_traits: {:?}\n",
                    profile.personality_traits
                ));
            }
        }
        let project_ids = member
            .project_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        block.push_str(&format!(
            "- project_ids: {:?}
",
            project_ids
        ));
    }
    block.push_str("
Use tyde_team_describe when you need the current roster and tyde_team_message_member to delegate to reports.

User request:
");
    block.push_str(&prompt);
    block
}

fn team_member_primary_project_id(member: &TeamMember) -> Result<ProjectId, String> {
    member
        .project_ids
        .first()
        .cloned()
        .ok_or_else(|| format!("team member {} has no project_ids", member.id))
}

fn debug_mcp_url_for_repo_root(base_url: &str, repo_root: &str) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!(
        "{base_url}{separator}repo_root={}",
        percent_encode_query_component(repo_root)
    )
}

pub(crate) fn mcp_url_for_agent(base_url: &str, agent_id: &AgentId) -> String {
    let separator = if base_url.contains('?') { '&' } else { '?' };
    format!(
        "{base_url}{separator}agent_id={}",
        percent_encode_query_component(&agent_id.0)
    )
}

fn validate_workflow_trigger_inputs(
    operation: &'static str,
    specs: &[WorkflowInputSpec],
    mut provided: HashMap<String, serde_json::Value>,
    workflow_id: &protocol::WorkflowId,
) -> AppResult<HashMap<String, serde_json::Value>> {
    let declared = specs
        .iter()
        .map(|input| input.id.as_str())
        .collect::<HashSet<_>>();
    for key in provided.keys() {
        if !declared.contains(key.as_str()) {
            return Err(AppError::invalid(
                operation,
                format!("unknown input {key:?} for workflow {workflow_id}"),
            ));
        }
    }

    let mut effective = HashMap::new();
    for spec in specs {
        let value = provided.remove(&spec.id).or_else(|| spec.default.clone());
        let Some(value) = value else {
            if spec.required {
                return Err(AppError::invalid(
                    operation,
                    format!(
                        "missing required input {:?} for workflow {workflow_id}",
                        spec.id
                    ),
                ));
            }
            continue;
        };
        if !workflow_input_value_matches(spec, &value) {
            return Err(AppError::invalid(
                operation,
                format!(
                    "input {:?} for workflow {workflow_id} must be {}",
                    spec.id,
                    workflow_input_expected_value(spec)
                ),
            ));
        }
        effective.insert(spec.id.clone(), value);
    }

    Ok(effective)
}

fn workflow_input_value_matches(spec: &WorkflowInputSpec, value: &serde_json::Value) -> bool {
    match spec.control {
        WorkflowInputControl::Text
        | WorkflowInputControl::MultilineText
        | WorkflowInputControl::FilePath => value.is_string(),
        WorkflowInputControl::Boolean => value.is_boolean(),
        WorkflowInputControl::Number => value.is_number(),
        WorkflowInputControl::Select => value
            .as_str()
            .is_some_and(|selected| spec.options.iter().any(|option| option.value == selected)),
    }
}

fn workflow_input_expected_value(spec: &WorkflowInputSpec) -> String {
    match spec.control {
        WorkflowInputControl::Text => "a string".to_owned(),
        WorkflowInputControl::MultilineText => "a string".to_owned(),
        WorkflowInputControl::FilePath => "a string".to_owned(),
        WorkflowInputControl::Boolean => "a boolean".to_owned(),
        WorkflowInputControl::Number => "a number".to_owned(),
        WorkflowInputControl::Select => {
            let options = spec
                .options
                .iter()
                .map(|option| format!("{:?}", option.value))
                .collect::<Vec<_>>()
                .join(", ");
            format!("one of the select option values: {options}")
        }
    }
}

fn is_workflow_terminal(status: WorkflowRunSnapshotStatus) -> bool {
    matches!(
        status,
        WorkflowRunSnapshotStatus::Completed
            | WorkflowRunSnapshotStatus::Failed
            | WorkflowRunSnapshotStatus::Cancelled
    )
}

fn workflow_location_for_scope(scope: WorkflowSourceScope) -> WorkflowCatalogLocation {
    let directory = match &scope {
        WorkflowSourceScope::Global => global_workflows_dir(),
        WorkflowSourceScope::Project { root, .. } => project_workflows_dir(root),
    };
    WorkflowCatalogLocation {
        scope,
        directory: directory.display().to_string(),
        exists: directory.is_dir(),
    }
}

fn validate_workflow_filename(filename: &str) -> Result<&str, String> {
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return Err("filename must not be empty".to_owned());
    }
    if trimmed != filename {
        return Err("filename must not contain surrounding whitespace".to_owned());
    }
    if !filename.ends_with(".md") {
        return Err("filename must end with .md".to_owned());
    }
    if filename.contains('/') || filename.contains('\\') {
        return Err("filename must be a basename, not a path".to_owned());
    }
    let mut components = Path::new(filename).components();
    let Some(Component::Normal(component)) = components.next() else {
        return Err("filename must be a normal basename".to_owned());
    };
    if components.next().is_some() || component.to_string_lossy() != filename {
        return Err("filename must be a normal basename".to_owned());
    }
    if filename == ".." {
        return Err("filename must not contain .. components".to_owned());
    }
    Ok(filename)
}

fn atomic_write_workflow(path: &Path, bytes: &[u8], replace: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("workflow path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create workflow directory '{}': {error}",
            parent.display()
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("workflow path {} has no filename", path.display()))?;
    let temp_path = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4()));
    let write_result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| {
                format!(
                    "failed to create temporary workflow file '{}': {error}",
                    temp_path.display()
                )
            })?;
        use std::io::Write;
        file.write_all(bytes).map_err(|error| {
            format!(
                "failed to write temporary workflow file '{}': {error}",
                temp_path.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "failed to flush temporary workflow file '{}': {error}",
                temp_path.display()
            )
        })?;
        if replace {
            std::fs::rename(&temp_path, path).map_err(|error| {
                format!(
                    "failed to replace workflow file '{}' with '{}': {error}",
                    path.display(),
                    temp_path.display()
                )
            })
        } else {
            std::fs::hard_link(&temp_path, path).map_err(|error| {
                format!(
                    "failed to create workflow file '{}' from '{}': {error}",
                    path.display(),
                    temp_path.display()
                )
            })?;
            if let Err(error) = std::fs::remove_file(&temp_path) {
                tracing::warn!(
                    path = %temp_path.display(),
                    error = %error,
                    "failed to remove temporary workflow file after create"
                );
            }
            Ok(())
        }
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    write_result
}

fn workflow_status_label(status: WorkflowRunSnapshotStatus) -> &'static str {
    match status {
        WorkflowRunSnapshotStatus::Running => "running",
        WorkflowRunSnapshotStatus::Completed => "completed",
        WorkflowRunSnapshotStatus::Failed => "failed",
        WorkflowRunSnapshotStatus::Cancelled => "cancelled",
    }
}

fn parse_workflow_agent_id(value: &str) -> Result<AgentId, String> {
    let trimmed = value.trim();
    Uuid::parse_str(trimmed).map_err(|_| format!("invalid workflow agent_id {value:?}"))?;
    Ok(AgentId(trimmed.to_owned()))
}

fn percent_encode_query_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }
    encoded
}

struct AnnotationTargetResolver {
    live_sessions: HashMap<AgentId, Option<SessionId>>,
    agent_by_session: HashMap<SessionId, AgentId>,
    children_by_parent: HashMap<AgentId, Vec<AgentId>>,
}

impl AnnotationTargetResolver {
    fn new(state: &HostState) -> Self {
        let mut live_sessions = HashMap::new();
        let mut agent_by_session = HashMap::new();
        let mut children_by_parent = HashMap::<AgentId, Vec<AgentId>>::new();
        for agent_id in state.registry.agent_ids() {
            let snapshot = state
                .registry
                .agent_handle(&agent_id)
                .map(|agent| agent.snapshot());
            let session_id = state.agent_sessions.get(&agent_id).cloned();
            if let Some(session_id) = session_id.clone() {
                agent_by_session.insert(session_id, agent_id.clone());
            }
            if let Some(parent_agent_id) = snapshot.and_then(|start| start.parent_agent_id) {
                children_by_parent
                    .entry(parent_agent_id)
                    .or_default()
                    .push(agent_id.clone());
            }
            live_sessions.insert(agent_id, session_id);
        }
        Self {
            live_sessions,
            agent_by_session,
            children_by_parent,
        }
    }

    fn canonicalize(
        &self,
        target: AgentAnnotationTarget,
    ) -> Result<Option<AgentAnnotationTarget>, String> {
        match target {
            AgentAnnotationTarget::Session {
                host_id,
                session_id,
            } => {
                ensure_non_empty_annotation_field(
                    "agent annotation target host_id",
                    host_id.0.as_str(),
                )?;
                ensure_non_empty_annotation_field(
                    "agent annotation target session_id",
                    session_id.0.as_str(),
                )?;
                Ok(Some(AgentAnnotationTarget::Session {
                    host_id,
                    session_id,
                }))
            }
            AgentAnnotationTarget::TransientAgent { host_id, agent_id } => {
                ensure_non_empty_annotation_field(
                    "agent annotation target host_id",
                    host_id.0.as_str(),
                )?;
                ensure_non_empty_annotation_field(
                    "agent annotation target agent_id",
                    agent_id.0.as_str(),
                )?;
                if host_id.0 != LOCAL_HOST_ID {
                    return Ok(None);
                }
                let Some(session_id) = self.live_sessions.get(&agent_id) else {
                    return Ok(None);
                };
                match session_id {
                    Some(session_id) => Ok(Some(AgentAnnotationTarget::Session {
                        host_id,
                        session_id: session_id.clone(),
                    })),
                    None => Ok(Some(AgentAnnotationTarget::TransientAgent {
                        host_id,
                        agent_id,
                    })),
                }
            }
        }
    }

    fn expand_parent_targets(
        &self,
        targets: Vec<AgentAnnotationTarget>,
    ) -> Vec<AgentAnnotationTarget> {
        let mut expanded = Vec::new();
        let mut seen = HashSet::new();
        for target in targets {
            let host_id = match &target {
                AgentAnnotationTarget::Session { host_id, .. }
                | AgentAnnotationTarget::TransientAgent { host_id, .. } => host_id.clone(),
            };
            let parent_agent_id = self.agent_id_for_target(&target);
            push_unique_annotation_target(&mut expanded, &mut seen, target);
            if host_id.0 != LOCAL_HOST_ID {
                continue;
            }
            if let Some(parent_agent_id) = parent_agent_id {
                let mut visited = HashSet::new();
                visited.insert(parent_agent_id.clone());
                self.push_descendant_targets(
                    &host_id,
                    &parent_agent_id,
                    &mut expanded,
                    &mut seen,
                    &mut visited,
                );
            }
        }
        expanded
    }

    fn push_descendant_targets(
        &self,
        host_id: &HostFilterId,
        parent_agent_id: &AgentId,
        expanded: &mut Vec<AgentAnnotationTarget>,
        seen: &mut HashSet<AgentAnnotationTarget>,
        visited: &mut HashSet<AgentId>,
    ) {
        let Some(children) = self.children_by_parent.get(parent_agent_id) else {
            return;
        };
        for child_id in children {
            if !visited.insert(child_id.clone()) {
                continue;
            }
            push_unique_annotation_target(
                expanded,
                seen,
                self.target_for_agent_id(host_id, child_id),
            );
            self.push_descendant_targets(host_id, child_id, expanded, seen, visited);
        }
    }

    fn agent_id_for_target(&self, target: &AgentAnnotationTarget) -> Option<AgentId> {
        match target {
            AgentAnnotationTarget::TransientAgent { host_id, agent_id }
                if host_id.0 == LOCAL_HOST_ID =>
            {
                self.live_sessions
                    .contains_key(agent_id)
                    .then(|| agent_id.clone())
            }
            AgentAnnotationTarget::Session {
                host_id,
                session_id,
            } if host_id.0 == LOCAL_HOST_ID => self.agent_by_session.get(session_id).cloned(),
            _ => None,
        }
    }

    fn target_for_agent_id(
        &self,
        host_id: &HostFilterId,
        agent_id: &AgentId,
    ) -> AgentAnnotationTarget {
        match self.live_sessions.get(agent_id).and_then(Clone::clone) {
            Some(session_id) => AgentAnnotationTarget::Session {
                host_id: host_id.clone(),
                session_id,
            },
            None => AgentAnnotationTarget::TransientAgent {
                host_id: host_id.clone(),
                agent_id: agent_id.clone(),
            },
        }
    }
}

fn push_unique_annotation_target(
    targets: &mut Vec<AgentAnnotationTarget>,
    seen: &mut HashSet<AgentAnnotationTarget>,
    target: AgentAnnotationTarget,
) {
    if seen.insert(target.clone()) {
        targets.push(target);
    }
}

fn ensure_non_empty_annotation_field(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(())
}

fn canonicalize_agent_tags_update(
    update: AgentTagsUpdate,
    resolver: &AnnotationTargetResolver,
) -> Result<AgentTagsUpdate, String> {
    match update {
        AgentTagsUpdate::AssignTag { target, tag_id } => {
            let original = target.clone();
            let target = resolver.canonicalize(target)?.unwrap_or(original);
            Ok(AgentTagsUpdate::AssignTag { target, tag_id })
        }
        AgentTagsUpdate::RemoveTag { target, tag_id } => {
            let original = target.clone();
            let target = resolver.canonicalize(target)?.unwrap_or(original);
            Ok(AgentTagsUpdate::RemoveTag { target, tag_id })
        }
        other => Ok(other),
    }
}

fn canonicalize_agent_pins_update(
    update: AgentPinsUpdate,
    resolver: &AnnotationTargetResolver,
) -> Result<AgentPinsUpdate, String> {
    match update {
        AgentPinsUpdate::Pin { target } => {
            let original = target.clone();
            let target = resolver.canonicalize(target)?.unwrap_or(original);
            Ok(AgentPinsUpdate::Pin { target })
        }
        AgentPinsUpdate::Unpin { target } => {
            let original = target.clone();
            let target = resolver.canonicalize(target)?.unwrap_or(original);
            Ok(AgentPinsUpdate::Unpin { target })
        }
    }
}

fn canonicalize_agent_groups_update(
    update: AgentGroupsUpdate,
    resolver: &AnnotationTargetResolver,
) -> Result<AgentGroupsUpdate, String> {
    match update {
        AgentGroupsUpdate::CreateGroup { name, targets } => Ok(AgentGroupsUpdate::CreateGroup {
            name,
            targets: resolver
                .expand_parent_targets(targets)
                .into_iter()
                .map(|target| {
                    let original = target.clone();
                    resolver
                        .canonicalize(target)
                        .map(|target| target.unwrap_or(original))
                })
                .collect::<Result<Vec<_>, _>>()?,
        }),
        AgentGroupsUpdate::MoveTargets { group_id, targets } => {
            Ok(AgentGroupsUpdate::MoveTargets {
                group_id,
                targets: resolver
                    .expand_parent_targets(targets)
                    .into_iter()
                    .map(|target| {
                        let original = target.clone();
                        resolver
                            .canonicalize(target)
                            .map(|target| target.unwrap_or(original))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }
        other => Ok(other),
    }
}

async fn complete_agents_view_preferences_snapshot(
    state: &HostState,
    snapshot: &mut AgentsViewPreferencesSnapshot,
) {
    let system_tags = compute_system_tags_snapshot(state).await;
    snapshot.tags.system = system_tags.system;
    snapshot.tags.system_assignments = system_tags.system_assignments;
}

async fn compute_system_tags_snapshot(state: &HostState) -> AgentTagsSnapshot {
    let projects = state
        .project_store
        .lock()
        .await
        .list()
        .unwrap_or_else(|error| {
            tracing::warn!(
                error = %error,
                "failed to list projects while computing system agent tags"
            );
            Vec::new()
        });
    let project_names = projects
        .into_iter()
        .map(|project| (project.id, project.name))
        .collect::<HashMap<_, _>>();

    let mut descriptors = HashMap::<AgentSystemTagId, AgentSystemTagDescriptor>::new();
    let mut assignments = HashMap::<AgentAnnotationTarget, HashSet<AgentSystemTagId>>::new();

    for agent_id in state.registry.agent_ids() {
        let Some(agent) = state.registry.agent_handle(&agent_id) else {
            continue;
        };
        let start = agent.snapshot();
        let target = agent_annotation_target_for_start(state, &start);
        let mut tag_ids = Vec::new();

        let (origin_id, origin_label) = origin_system_tag(start.origin);
        descriptors
            .entry(origin_id.clone())
            .or_insert_with(|| system_tag_descriptor(origin_id.clone(), origin_label));
        tag_ids.push(origin_id);

        let (backend_id, backend_label) = backend_system_tag(start.backend_kind);
        descriptors
            .entry(backend_id.clone())
            .or_insert_with(|| system_tag_descriptor(backend_id.clone(), backend_label));
        tag_ids.push(backend_id);

        if let Some(project_id) = start.project_id.as_ref() {
            let (project_id_tag, project_label) =
                project_system_tag(project_id, project_names.get(project_id));
            descriptors
                .entry(project_id_tag.clone())
                .or_insert_with(|| system_tag_descriptor(project_id_tag.clone(), project_label));
            tag_ids.push(project_id_tag);
        }

        assignments.entry(target).or_default().extend(tag_ids);
    }

    let mut system = descriptors.into_values().collect::<Vec<_>>();
    system.sort_by(|left, right| left.id.0.cmp(&right.id.0));

    let mut system_assignments = assignments
        .into_iter()
        .map(|(target, tag_ids)| {
            let mut tag_ids = tag_ids.into_iter().collect::<Vec<_>>();
            tag_ids.sort_by(|left, right| left.0.cmp(&right.0));
            AgentSystemTagAssignment { target, tag_ids }
        })
        .collect::<Vec<_>>();
    system_assignments
        .sort_by(|left, right| compare_annotation_targets(&left.target, &right.target));

    AgentTagsSnapshot {
        manual: Vec::new(),
        system,
        manual_assignments: Vec::new(),
        system_assignments,
    }
}

fn agent_annotation_target_for_start(
    state: &HostState,
    start: &AgentStartPayload,
) -> AgentAnnotationTarget {
    let host_id = HostFilterId(LOCAL_HOST_ID.to_owned());
    if let Some(session_id) = state.agent_sessions.get(&start.agent_id).cloned() {
        AgentAnnotationTarget::Session {
            host_id,
            session_id,
        }
    } else {
        AgentAnnotationTarget::TransientAgent {
            host_id,
            agent_id: start.agent_id.clone(),
        }
    }
}

fn system_tag_descriptor(id: AgentSystemTagId, name: String) -> AgentSystemTagDescriptor {
    AgentSystemTagDescriptor {
        id,
        name,
        color: None,
    }
}

fn origin_system_tag(origin: AgentOrigin) -> (AgentSystemTagId, String) {
    match origin {
        AgentOrigin::User => (
            AgentSystemTagId("system:origin:user".to_owned()),
            "User".to_owned(),
        ),
        AgentOrigin::AgentControl => (
            AgentSystemTagId("system:origin:agent-control".to_owned()),
            "Agent control".to_owned(),
        ),
        AgentOrigin::SideQuestion => (
            AgentSystemTagId("system:origin:side-quest".to_owned()),
            "Side quest".to_owned(),
        ),
        AgentOrigin::BackendNative => (
            AgentSystemTagId("system:origin:sub-agent".to_owned()),
            "Sub-agent".to_owned(),
        ),
        AgentOrigin::TeamMember => (
            AgentSystemTagId("system:origin:team".to_owned()),
            "Team".to_owned(),
        ),
        AgentOrigin::Workflow => (
            AgentSystemTagId("system:origin:workflow".to_owned()),
            "Workflow".to_owned(),
        ),
    }
}

fn backend_system_tag(backend: BackendKind) -> (AgentSystemTagId, String) {
    match backend {
        BackendKind::Tycode => (
            AgentSystemTagId("system:backend:tycode".to_owned()),
            "Tycode".to_owned(),
        ),
        BackendKind::Kiro => (
            AgentSystemTagId("system:backend:kiro".to_owned()),
            "Kiro".to_owned(),
        ),
        BackendKind::Claude => (
            AgentSystemTagId("system:backend:claude".to_owned()),
            "Claude".to_owned(),
        ),
        BackendKind::Codex => (
            AgentSystemTagId("system:backend:codex".to_owned()),
            "Codex".to_owned(),
        ),
        BackendKind::Antigravity => (
            AgentSystemTagId("system:backend:antigravity".to_owned()),
            "Antigravity".to_owned(),
        ),
        BackendKind::Hermes => (
            AgentSystemTagId("system:backend:hermes".to_owned()),
            "Hermes".to_owned(),
        ),
    }
}

fn project_system_tag(
    project_id: &ProjectId,
    project_name: Option<&String>,
) -> (AgentSystemTagId, String) {
    (
        AgentSystemTagId(format!("system:project:{}", project_id.0)),
        project_name
            .cloned()
            .unwrap_or_else(|| format!("Project {}", project_id.0)),
    )
}

fn compare_annotation_targets(
    left: &AgentAnnotationTarget,
    right: &AgentAnnotationTarget,
) -> std::cmp::Ordering {
    annotation_target_key(left).cmp(&annotation_target_key(right))
}

fn annotation_target_key(target: &AgentAnnotationTarget) -> (u8, &str, &str) {
    match target {
        AgentAnnotationTarget::Session {
            host_id,
            session_id,
        } => (0, host_id.0.as_str(), session_id.0.as_str()),
        AgentAnnotationTarget::TransientAgent { host_id, agent_id } => {
            (1, host_id.0.as_str(), agent_id.0.as_str())
        }
    }
}

fn emit_or_queue_host_frame(
    subscriber: &mut HostSubscriber,
    kind: FrameKind,
    payload: serde_json::Value,
) -> Result<(), StreamClosed> {
    if subscriber.bootstrapped {
        subscriber.stream.send_value(kind, payload)
    } else {
        subscriber.pending_bootstrap_frames.push((kind, payload));
        Ok(())
    }
}

fn prepare_new_agent_fanout_for_subscriber(
    subscriber: &mut HostSubscriber,
    start: &AgentStartPayload,
    agent_handle: &AgentHandle,
    activity_summary: AgentActivitySummaryState,
) -> Option<(Stream, bool, StreamPath, AgentActivitySummaryState)> {
    let instance_stream = new_instance_stream(&start.agent_id);
    subscriber
        .known_agent_streams
        .insert(instance_stream.clone());
    let attach_eagerly = matches!(subscriber.agent_replay, AgentReplayMode::Eager);
    if attach_eagerly {
        subscriber
            .attached_agent_streams
            .insert(instance_stream.clone());
    }

    if subscriber.bootstrapped {
        Some((
            subscriber.stream.clone(),
            attach_eagerly,
            instance_stream,
            activity_summary,
        ))
    } else {
        subscriber
            .pending_bootstrap_new_agents
            .push(PendingNewAgentFanout {
                start: start.clone(),
                agent_handle: agent_handle.clone(),
                instance_stream,
                attach_eagerly,
                activity_summary,
            });
        None
    }
}

fn forget_agent_fanout_for_subscriber(subscriber: &mut HostSubscriber, agent_id: &AgentId) {
    let instance_stream = new_instance_stream(agent_id);
    subscriber.known_agent_streams.remove(&instance_stream);
    subscriber.attached_agent_streams.remove(&instance_stream);
    subscriber
        .bootstrapped_agent_streams
        .remove(&instance_stream);
    subscriber
        .pending_bootstrap_new_agents
        .retain(|pending| pending.start.agent_id != *agent_id);
}

fn emit_new_agent_for_stream(
    start: &AgentStartPayload,
    agent_handle: &AgentHandle,
    stream: &Stream,
    instance_stream: StreamPath,
    attach_eagerly: bool,
    activity_summary: AgentActivitySummaryState,
) -> Result<Option<DeferredAgentAttachment>, StreamClosed> {
    let new_agent = NewAgentPayload {
        agent_id: start.agent_id.clone(),
        name: start.name.clone(),
        origin: start.origin,
        backend_kind: start.backend_kind,
        launch_profile_id: start.launch_profile_id.clone(),
        workspace_roots: start.workspace_roots.clone(),
        custom_agent_id: start.custom_agent_id.clone(),
        team_id: start.team_id.clone(),
        team_member_id: start.team_member_id.clone(),
        project_id: start.project_id.clone(),
        parent_agent_id: start.parent_agent_id.clone(),
        session_id: start.session_id.clone(),
        workflow: start.workflow.clone(),
        created_at_ms: start.created_at_ms,
        instance_stream: instance_stream.clone(),
        activity_summary,
    };

    let payload = serde_json::to_value(&new_agent)
        .expect("failed to serialize NewAgent payload for host stream fanout");
    stream.send_value(FrameKind::NewAgent, payload)?;

    let attachment = attach_eagerly.then(|| {
        let agent_stream = stream.with_path(instance_stream.clone());
        DeferredAgentAttachment {
            host_stream: stream.path().clone(),
            agent_stream: instance_stream,
            reply: agent_handle.begin_attach(agent_stream),
            agent_handle: None,
            stream: None,
        }
    });

    Ok(attachment)
}

#[derive(Clone, Copy)]
struct SessionPageRequest {
    scope: SessionListScope,
    cursor: SessionListCursor,
    limit: Option<u32>,
}

impl SessionPageRequest {
    fn initial(
        generation: SessionListGeneration,
        scope: SessionListScope,
        mode: SessionListReplayMode,
        total_count: usize,
        limit: Option<u32>,
        operation: &'static str,
    ) -> AppResult<Self> {
        Ok(Self {
            scope,
            cursor: SessionListCursor {
                generation,
                offset: 0,
            },
            limit: session_page_limit(limit, mode, total_count, operation)?,
        })
    }

    fn continuing(
        cursor: SessionListCursor,
        scope: SessionListScope,
        limit: Option<u32>,
        mode: SessionListReplayMode,
        total_count: usize,
        operation: &'static str,
    ) -> AppResult<Self> {
        Ok(Self {
            scope,
            cursor,
            limit: session_page_limit(limit, mode, total_count, operation)?,
        })
    }
}

fn session_page_limit(
    limit: Option<u32>,
    mode: SessionListReplayMode,
    total_count: usize,
    operation: &'static str,
) -> AppResult<Option<u32>> {
    match limit {
        Some(0) => Err(AppError::invalid(
            operation,
            "session list limit must be greater than zero",
        )),
        Some(limit) if limit > MAX_SESSION_LIST_PAGE_LIMIT => Err(AppError::invalid(
            operation,
            format!("session list limit {limit} exceeds maximum {MAX_SESSION_LIST_PAGE_LIMIT}"),
        )),
        Some(limit) => Ok(Some(limit)),
        None => match mode {
            SessionListReplayMode::Full => Ok(u32::try_from(total_count).ok()),
            SessionListReplayMode::Paged { limit } => Ok(Some(limit)),
        },
    }
}

fn replace_session_list_snapshot(
    subscriber: &mut HostSubscriber,
    scope: SessionListScope,
    sessions: Vec<SessionSummary>,
    limit: Option<u32>,
    operation: &'static str,
) -> AppResult<(Vec<SessionSummary>, SessionListPageInfo)> {
    let generation = subscriber
        .session_list_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.generation.0.checked_add(1))
        .map(SessionListGeneration)
        .unwrap_or(SessionListGeneration(1));
    let request = SessionPageRequest::initial(
        generation,
        scope,
        subscriber.session_list_replay,
        sessions.len(),
        limit,
        operation,
    )?;
    subscriber.session_list_snapshot = Some(SessionListSnapshot {
        generation,
        scope,
        sessions,
    });
    let snapshot = subscriber
        .session_list_snapshot
        .as_ref()
        .expect("session list snapshot was just stored");
    page_session_summaries(&snapshot.sessions, request).map_err(|message| {
        AppError::invalid(
            operation,
            format!("failed to page session list snapshot: {message}"),
        )
    })
}

fn page_existing_session_list_snapshot(
    subscriber: &HostSubscriber,
    cursor: SessionListCursor,
    scope: Option<SessionListScope>,
    limit: Option<u32>,
    operation: &'static str,
) -> AppResult<(Vec<SessionSummary>, SessionListPageInfo)> {
    let snapshot = subscriber.session_list_snapshot.as_ref().ok_or_else(|| {
        AppError::invalid(
            operation,
            "session list cursor cannot be used before a session list snapshot exists",
        )
    })?;
    if cursor.generation != snapshot.generation {
        return Err(AppError::invalid(
            operation,
            format!(
                "stale session list cursor generation {}; current generation is {}",
                cursor.generation.0, snapshot.generation.0
            ),
        ));
    }
    if let Some(scope) = scope
        && scope != snapshot.scope
    {
        return Err(AppError::invalid(
            operation,
            format!(
                "session list cursor scope {scope:?} does not match snapshot scope {:?}",
                snapshot.scope
            ),
        ));
    }
    let request = SessionPageRequest::continuing(
        cursor,
        snapshot.scope,
        limit,
        subscriber.session_list_replay,
        snapshot.sessions.len(),
        operation,
    )?;
    page_session_summaries(&snapshot.sessions, request).map_err(|message| {
        AppError::invalid(
            operation,
            format!("failed to page session list snapshot: {message}"),
        )
    })
}

fn page_session_summaries(
    sessions: &[SessionSummary],
    request: SessionPageRequest,
) -> Result<(Vec<SessionSummary>, SessionListPageInfo), String> {
    let total_count = u32::try_from(sessions.len())
        .map_err(|_| "session count exceeds protocol page counter range".to_owned())?;
    let start = usize::try_from(request.cursor.offset)
        .map_err(|_| "session cursor exceeds host pointer range".to_owned())?;
    if start > sessions.len() {
        return Err(format!(
            "session cursor {} is beyond total session count {}",
            request.cursor.offset, total_count
        ));
    }

    let remaining = sessions.len().saturating_sub(start);
    let requested_limit = match request.limit {
        Some(limit) => limit,
        None => u32::try_from(remaining)
            .map_err(|_| "remaining session count exceeds protocol page range".to_owned())?,
    };
    let limit = usize::try_from(requested_limit)
        .map_err(|_| "session page limit exceeds host pointer range".to_owned())?;
    let end = start.saturating_add(limit).min(sessions.len());
    let page_sessions = sessions[start..end].to_vec();
    let status = if end < sessions.len() {
        let offset = u32::try_from(end)
            .map_err(|_| "next session cursor exceeds protocol range".to_owned())?;
        let next_cursor = SessionListCursor {
            generation: request.cursor.generation,
            offset,
        };
        SessionListPageStatus::More { next_cursor }
    } else {
        SessionListPageStatus::Complete
    };

    Ok((
        page_sessions,
        SessionListPageInfo {
            scope: request.scope,
            cursor: request.cursor,
            limit: requested_limit,
            total_count,
            status,
        },
    ))
}

async fn fan_out_session_lists(state: &mut HostState) {
    let sessions = state
        .session_store
        .lock()
        .await
        .summaries_for_scope_with_antigravity_conversations_dir(
            SessionListScope::AllSessions,
            &state.antigravity_conversations_dir,
        )
        .unwrap_or_else(|err| panic!("failed to list sessions for fanout: {err}"));

    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        let scope = subscriber
            .session_list_snapshot
            .as_ref()
            .map(|snapshot| snapshot.scope)
            .unwrap_or_else(|| subscriber.session_list_replay.default_scope());
        let scoped_sessions = sessions
            .iter()
            .filter(|summary| session_summary_matches_scope(summary, scope))
            .cloned()
            .collect::<Vec<_>>();
        let (page_sessions, page) = replace_session_list_snapshot(
            subscriber,
            scope,
            scoped_sessions,
            None,
            "session_list_fanout",
        )
        .unwrap_or_else(|err| panic!("failed to page sessions for fanout: {err}"));
        let payload = serde_json::to_value(SessionListPayload {
            sessions: page_sessions,
            page,
        })
        .expect("failed to serialize SessionList payload for host stream fanout");
        if emit_or_queue_host_frame(subscriber, FrameKind::SessionList, payload.clone()).is_err() {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_task_token_usages(state: &mut HostState, payloads: Vec<TaskTokenUsagePayload>) {
    if payloads.is_empty() {
        return;
    }
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        for payload in &payloads {
            if emit_task_token_usage_for_subscriber(payload, subscriber)
                .await
                .is_err()
            {
                dead_paths.push(path.clone());
                break;
            }
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

fn task_token_usage_sources_for_state(
    state: &HostState,
) -> (
    Vec<AgentHandle>,
    Vec<AgentUsageSnapshot>,
    HashSet<AgentId>,
    HashMap<AgentId, SessionId>,
) {
    let live_agent_ids = state
        .registry
        .agent_ids()
        .into_iter()
        .collect::<HashSet<_>>();
    let handles = state
        .registry
        .agent_ids()
        .into_iter()
        .filter_map(|agent_id| state.registry.agent_handle(&agent_id))
        .collect::<Vec<_>>();
    (
        handles,
        state
            .closed_agent_usage_snapshots
            .values()
            .filter(|snapshot| !live_agent_ids.contains(&snapshot.start.agent_id))
            .cloned()
            .collect(),
        live_agent_ids,
        state.agent_sessions.clone(),
    )
}

async fn task_token_usage_rollups_from_handles(
    handles: Vec<AgentHandle>,
    closed_snapshots: Vec<AgentUsageSnapshot>,
    live_agent_ids: &HashSet<AgentId>,
    agent_sessions: &HashMap<AgentId, SessionId>,
) -> Vec<TaskTokenUsagePayload> {
    let mut snapshots = Vec::new();
    for handle in handles {
        snapshots.push(read_agent_usage_snapshot_or_unavailable(&handle).await);
    }
    snapshots.extend(closed_snapshots);
    task_token_usage_rollups_from_snapshots(snapshots, live_agent_ids, agent_sessions)
}

async fn read_agent_usage_snapshot_or_unavailable(handle: &AgentHandle) -> AgentUsageSnapshot {
    let start = handle.snapshot();
    handle
        .read_usage_snapshot()
        .await
        .unwrap_or_else(|| unavailable_agent_usage_snapshot(start))
}

fn unavailable_agent_usage_snapshot(start: AgentStartPayload) -> AgentUsageSnapshot {
    AgentUsageSnapshot {
        start,
        usage: TaskTokenUsageScope::Unavailable {
            reason: TaskTokenUsageUnavailableReason::AgentUnavailable,
        },
        model: None,
    }
}

fn task_token_usage_rollups_from_snapshots(
    snapshots: Vec<AgentUsageSnapshot>,
    live_agent_ids: &HashSet<AgentId>,
    agent_sessions: &HashMap<AgentId, SessionId>,
) -> Vec<TaskTokenUsagePayload> {
    let snapshots_by_id = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.start.agent_id.clone(), snapshot))
        .collect::<HashMap<_, _>>();
    let mut children_by_parent: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    for snapshot in snapshots_by_id.values() {
        if let Some(parent_agent_id) = &snapshot.start.parent_agent_id
            && snapshots_by_id.contains_key(parent_agent_id)
        {
            children_by_parent
                .entry(parent_agent_id.clone())
                .or_default()
                .push(snapshot.start.agent_id.clone());
        }
    }
    for children in children_by_parent.values_mut() {
        children.sort_by(|left, right| {
            let left_start = &snapshots_by_id
                .get(left)
                .expect("child id must have snapshot")
                .start;
            let right_start = &snapshots_by_id
                .get(right)
                .expect("child id must have snapshot")
                .start;
            (left_start.created_at_ms, left.0.as_str())
                .cmp(&(right_start.created_at_ms, right.0.as_str()))
        });
    }

    let mut roots = snapshots_by_id
        .values()
        .filter(|snapshot| {
            live_agent_ids.contains(&snapshot.start.agent_id)
                && snapshot
                    .start
                    .parent_agent_id
                    .as_ref()
                    .is_none_or(|parent| !snapshots_by_id.contains_key(parent))
        })
        .map(|snapshot| snapshot.start.agent_id.clone())
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        let left_start = &snapshots_by_id
            .get(left)
            .expect("root id must have snapshot")
            .start;
        let right_start = &snapshots_by_id
            .get(right)
            .expect("root id must have snapshot")
            .start;
        (left_start.created_at_ms, left.0.as_str())
            .cmp(&(right_start.created_at_ms, right.0.as_str()))
    });

    roots
        .into_iter()
        .filter_map(|root_id| {
            task_token_usage_rollup_for_root(
                &root_id,
                &snapshots_by_id,
                &children_by_parent,
                agent_sessions,
            )
        })
        .collect()
}

fn task_token_usage_rollup_for_root(
    root_id: &AgentId,
    snapshots_by_id: &HashMap<AgentId, AgentUsageSnapshot>,
    children_by_parent: &HashMap<AgentId, Vec<AgentId>>,
    agent_sessions: &HashMap<AgentId, SessionId>,
) -> Option<TaskTokenUsagePayload> {
    let root = snapshots_by_id.get(root_id)?;
    let mut ordered = Vec::new();
    let mut visited = HashSet::new();
    collect_task_token_usage_entries(
        root_id,
        0,
        snapshots_by_id,
        children_by_parent,
        agent_sessions,
        &mut visited,
        &mut ordered,
    );

    let descendant_count = ordered.len().saturating_sub(1).min(u32::MAX as usize) as u32;
    let total = aggregate_task_token_usage(ordered.iter().map(|entry| &entry.usage));
    let descendant_usage =
        aggregate_task_token_usage(ordered.iter().skip(1).map(|entry| &entry.usage));
    Some(TaskTokenUsagePayload {
        root_agent_id: root.start.agent_id.clone(),
        root_session_id: agent_session_id(&root.start, agent_sessions),
        total,
        self_usage: ordered.first().map(|entry| entry.usage.clone()).unwrap_or(
            TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::AgentUnavailable,
            },
        ),
        descendant_usage,
        descendant_count,
        breakdown: ordered,
    })
}

fn collect_task_token_usage_entries(
    agent_id: &AgentId,
    depth: u32,
    snapshots_by_id: &HashMap<AgentId, AgentUsageSnapshot>,
    children_by_parent: &HashMap<AgentId, Vec<AgentId>>,
    agent_sessions: &HashMap<AgentId, SessionId>,
    visited: &mut HashSet<AgentId>,
    ordered: &mut Vec<TaskTokenUsageEntry>,
) {
    if !visited.insert(agent_id.clone()) {
        return;
    }
    let Some(snapshot) = snapshots_by_id.get(agent_id) else {
        return;
    };
    let tree_index = ordered.len().min(u32::MAX as usize) as u32;
    let parent_session_id = snapshot
        .start
        .parent_agent_id
        .as_ref()
        .and_then(|parent| snapshots_by_id.get(parent).map(|snapshot| &snapshot.start))
        .and_then(|start| agent_session_id(start, agent_sessions));
    ordered.push(TaskTokenUsageEntry {
        agent_id: snapshot.start.agent_id.clone(),
        session_id: agent_session_id(&snapshot.start, agent_sessions),
        parent_agent_id: snapshot.start.parent_agent_id.clone(),
        parent_session_id,
        name: snapshot.start.name.clone(),
        origin: snapshot.start.origin,
        backend_kind: snapshot.start.backend_kind,
        model: snapshot.model.clone(),
        depth,
        tree_index,
        usage: snapshot.usage.clone(),
    });
    if let Some(children) = children_by_parent.get(agent_id) {
        for child in children {
            collect_task_token_usage_entries(
                child,
                depth.saturating_add(1),
                snapshots_by_id,
                children_by_parent,
                agent_sessions,
                visited,
                ordered,
            );
        }
    }
}

fn agent_session_id(
    start: &AgentStartPayload,
    agent_sessions: &HashMap<AgentId, SessionId>,
) -> Option<SessionId> {
    start
        .session_id
        .clone()
        .or_else(|| agent_sessions.get(&start.agent_id).cloned())
}

fn aggregate_task_token_usage<'a>(
    usages: impl Iterator<Item = &'a TaskTokenUsageScope>,
) -> TaskTokenUsageAggregate {
    let mut usage = TaskTokenUsageAmount::zero();
    let mut reported_count = 0_u32;
    let mut partial_seen = false;
    let mut reasons = Vec::new();
    let mut unavailable_count = 0_u32;

    for scope in usages {
        match scope {
            TaskTokenUsageScope::Known { usage: known } => {
                reported_count = reported_count.saturating_add(1);
                usage.saturating_add(known);
            }
            TaskTokenUsageScope::Partial {
                usage: partial,
                unavailable_count: partial_unavailable_count,
                reasons: partial_reasons,
            } => {
                reported_count = reported_count.saturating_add(1);
                partial_seen = true;
                usage.saturating_add(partial);
                unavailable_count = unavailable_count.saturating_add(*partial_unavailable_count);
                extend_task_token_usage_reasons(&mut reasons, partial_reasons);
            }
            TaskTokenUsageScope::Unavailable { reason } => {
                unavailable_count = unavailable_count.saturating_add(1);
                extend_task_token_usage_reasons(&mut reasons, &[*reason]);
            }
        }
    }
    reasons.sort();

    let status = if reported_count == 0 && unavailable_count > 0 {
        usage = TaskTokenUsageAmount::total_only(0);
        TaskTokenUsageStatus::Unavailable {
            unavailable_count,
            reasons,
        }
    } else if partial_seen || unavailable_count > 0 {
        TaskTokenUsageStatus::Partial {
            unavailable_count,
            reasons,
        }
    } else {
        TaskTokenUsageStatus::Known
    };
    TaskTokenUsageAggregate { usage, status }
}

fn extend_task_token_usage_reasons(
    reasons: &mut Vec<TaskTokenUsageUnavailableReason>,
    additions: &[TaskTokenUsageUnavailableReason],
) {
    for reason in additions {
        if !reasons.contains(reason) {
            reasons.push(*reason);
        }
    }
}

#[cfg(test)]
fn normalize_antigravity_session_resumability_with<F>(
    sessions: &mut [SessionSummary],
    is_antigravity_resumable: F,
) where
    F: Fn(&SessionId) -> bool,
{
    for session in sessions {
        if session.backend_kind == protocol::BackendKind::Antigravity {
            session.resumable = !antigravity_summary_is_permanently_non_resumable(session)
                && is_antigravity_resumable(&session.id);
        }
    }
}

#[cfg(test)]
fn antigravity_summary_is_permanently_non_resumable(session: &SessionSummary) -> bool {
    session.compacted_to_session_id.is_some() || (!session.resumable && session.parent_id.is_some())
}

async fn fan_out_project_notify(state: &mut HostState, payload: ProjectNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_project_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_custom_agent_notify(state: &mut HostState, payload: CustomAgentNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_custom_agent_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_steering_notify(state: &mut HostState, payload: SteeringNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_steering_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_skill_notify(state: &mut HostState, payload: SkillNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_skill_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_mcp_server_notify(state: &mut HostState, payload: McpServerNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_mcp_server_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_workflow_notify(state: &mut HostState, payload: WorkflowNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_workflow_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_workflow_run_notify(state: &mut HostState, run: WorkflowRunSnapshot) {
    let payload = WorkflowRunNotifyPayload { run };
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_workflow_run_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_registry_events(state: &mut HostState, events: TeamRegistryEvents) {
    for payload in events.team_notifies {
        fan_out_team_notify(state, payload).await;
    }
    for payload in events.member_notifies {
        fan_out_team_member_notify(state, payload).await;
    }
    for payload in events.binding_notifies {
        fan_out_team_member_binding_notify(state, payload).await;
    }
    for payload in events.draft_notifies {
        fan_out_team_draft_notify(state, payload).await;
    }
    for payload in events.shuffle_suggestion_notifies {
        fan_out_team_member_shuffle_suggestion(state, payload).await;
    }
}

async fn fan_out_team_member_shuffle_suggestion(
    state: &mut HostState,
    payload: TeamMemberShuffleSuggestionNotifyPayload,
) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_member_shuffle_suggestion_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_notify(state: &mut HostState, payload: TeamNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_member_notify(state: &mut HostState, payload: TeamMemberNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_member_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_member_binding_notify(
    state: &mut HostState,
    payload: TeamMemberBindingNotifyPayload,
) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_member_binding_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_team_draft_notify(state: &mut HostState, payload: TeamDraftNotifyPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_team_draft_notify_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn agent_team_validation_refs(
    state: &HostState,
    operation: &'static str,
) -> AppResult<AgentTeamValidationRefs> {
    let custom_agent_ids = state
        .custom_agent_store
        .lock()
        .await
        .list()
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?
        .into_iter()
        .map(|custom_agent| custom_agent.id)
        .collect::<HashSet<_>>();
    let project_ids = state
        .project_store
        .lock()
        .await
        .list()
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?
        .into_iter()
        .map(|project| project.id)
        .collect::<HashSet<_>>();
    let enabled_backend_kinds = state
        .settings_store
        .lock()
        .await
        .get()
        .map_err(|error| AppError::internal(operation, anyhow!(error)))?
        .enabled_backends
        .into_iter()
        .collect::<HashSet<_>>();
    let (role_preset_ids, personality_preset_ids) = team_preset_validation_refs();
    Ok(AgentTeamValidationRefs {
        custom_agent_ids,
        project_ids,
        enabled_backend_kinds,
        role_preset_ids,
        personality_preset_ids,
        legacy_backend_kind: None,
        purged_gemini_session_ids: HashSet::new(),
    })
}

fn project_stream_path(project_id: &ProjectId) -> StreamPath {
    StreamPath(format!("/project/{}", project_id.0))
}

async fn ensure_project_actor(
    state: &mut HostState,
    project_id: ProjectId,
) -> Result<ProjectStreamHandle, String> {
    if let Some(subscription) = state.project_streams.get(&project_id)
        && !subscription.task.is_finished()
    {
        return Ok(subscription.handle.clone());
    }

    if let Some(mut router) = state.code_intel_routers.remove(&project_id) {
        router.shutdown_all();
    }

    if let Some(subscription) = state.project_streams.remove(&project_id) {
        subscription.task.abort();
    }

    let subscription = spawn_project_subscription(
        Arc::clone(&state.project_store),
        project_id.clone(),
        state.review_registry.clone(),
    )
    .await?;
    let handle = subscription.handle.clone();
    state.project_streams.insert(project_id, subscription);
    Ok(handle)
}

async fn subscribe_host_to_project(
    state: &mut HostState,
    host_path: &StreamPath,
    project_id: ProjectId,
) -> Result<(), String> {
    let Some(subscriber) = state.host_streams.get(host_path) else {
        return Ok(());
    };
    let project_output_stream = subscriber
        .stream
        .with_path(project_stream_path(&project_id));
    let summaries = state.review_registry.summaries(project_id.clone()).await?;
    let handle = ensure_project_actor(state, project_id).await?;
    handle
        .add_subscriber(host_path.clone(), project_output_stream, summaries)
        .await
}

async fn emit_review_list_changed_for_project(
    state: &mut HostState,
    project_id: ProjectId,
) -> Result<(), String> {
    let summaries = state.review_registry.summaries(project_id.clone()).await?;
    let handle = ensure_project_actor(state, project_id).await?;
    handle
        .emit_project_event(protocol::ProjectEventPayload::ReviewListChanged { reviews: summaries })
        .await
}

async fn fan_out_host_settings(state: &mut HostState, settings: protocol::HostSettings) {
    if let Err(error) = sync_code_intel_router_roots(state).await {
        tracing::warn!(
            error = %error,
            "continuing code-intel settings fanout after router pruning failed"
        );
    }
    for router in state.code_intel_routers.values_mut() {
        router.update_settings(settings.code_intel.clone());
    }

    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_host_settings_for_subscriber(&settings, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn sync_code_intel_router_roots(state: &mut HostState) -> Result<(), String> {
    let projects = state
        .project_store
        .lock()
        .await
        .list()
        .map_err(|error| error.to_string())?;
    let roots_by_project = projects
        .into_iter()
        .map(|project| (project.id.clone(), project.root_paths()))
        .collect::<HashMap<_, _>>();
    let stale_projects = state
        .code_intel_routers
        .keys()
        .filter(|project_id| !roots_by_project.contains_key(*project_id))
        .cloned()
        .collect::<Vec<_>>();

    for (project_id, roots) in roots_by_project {
        if let Some(router) = state.code_intel_routers.get_mut(&project_id) {
            router.retain_roots(&roots);
        }
    }

    for project_id in stale_projects {
        if let Some(mut router) = state.code_intel_routers.remove(&project_id) {
            router.shutdown_all();
        }
    }

    Ok(())
}

async fn apply_agent_activity_summary_setting(
    state: &mut HostState,
    settings: &protocol::HostSettings,
) {
    let enabled = settings.background_agent_features.agent_activity_summaries;
    let current = *state.activity_summary_settings_tx.borrow();
    if current.enabled == enabled {
        return;
    }
    state.activity_summary_epoch = state.activity_summary_epoch.saturating_add(1);
    let signal = ActivitySummarySettingsSignal {
        enabled,
        epoch: state.activity_summary_epoch,
    };
    let _ = state.activity_summary_settings_tx.send(signal);
    if enabled {
        return;
    }

    let agent_ids = state.registry.agent_ids();
    for agent_id in agent_ids {
        let disabled = AgentActivitySummaryState::Disabled;
        if state.agent_activity_summaries.get(&agent_id) == Some(&disabled) {
            continue;
        }
        state
            .agent_activity_summaries
            .insert(agent_id.clone(), disabled.clone());
        fan_out_agent_activity_summary(
            state,
            AgentActivitySummaryPayload {
                agent_id,
                state: disabled,
            },
        )
        .await;
    }
}

fn apply_agent_supervisor_setting(state: &mut HostState, settings: &protocol::HostSettings) {
    let current = *state.supervisor_settings_tx.borrow();
    if current.settings == settings.supervisor {
        return;
    }
    state.supervisor_epoch = state.supervisor_epoch.saturating_add(1);
    let _ = state.supervisor_settings_tx.send(SupervisorSettingsSignal {
        settings: settings.supervisor,
        epoch: state.supervisor_epoch,
    });
}

fn initial_agent_activity_summary_state(
    state: &mut HostState,
    agent_id: &AgentId,
) -> AgentActivitySummaryState {
    if !state.activity_summary_settings_tx.borrow().enabled {
        return AgentActivitySummaryState::Disabled;
    }
    let summary_state = AgentActivitySummaryState::Empty;
    state
        .agent_activity_summaries
        .entry(agent_id.clone())
        .or_insert_with(|| summary_state.clone())
        .clone()
}

fn current_agent_activity_summary_state(
    state: &HostState,
    agent_id: &AgentId,
) -> AgentActivitySummaryState {
    state
        .agent_activity_summaries
        .get(agent_id)
        .cloned()
        .unwrap_or_else(|| {
            if state.activity_summary_settings_tx.borrow().enabled {
                AgentActivitySummaryState::Empty
            } else {
                AgentActivitySummaryState::Disabled
            }
        })
}

fn activity_summary_from_state(state: &AgentActivitySummaryState) -> Option<AgentActivitySummary> {
    match state {
        AgentActivitySummaryState::Fresh { summary }
        | AgentActivitySummaryState::Stale { summary, .. } => Some(summary.clone()),
        AgentActivitySummaryState::Pending { previous, .. }
        | AgentActivitySummaryState::Error { previous, .. } => previous.clone(),
        AgentActivitySummaryState::Disabled | AgentActivitySummaryState::Empty => None,
    }
}

async fn fan_out_agent_activity_summary(
    state: &mut HostState,
    payload: AgentActivitySummaryPayload,
) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_agent_activity_summary_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_agents_view_preferences(
    state: &mut HostState,
    snapshot: AgentsViewPreferencesSnapshot,
) {
    let payload = AgentsViewPreferencesNotifyPayload { snapshot };
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_agents_view_preferences_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_current_agents_view_preferences(state: &mut HostState) {
    let Some(store) = state.agents_view_preferences_store.clone() else {
        return;
    };
    let mut snapshot = store.lock().await.snapshot();
    complete_agents_view_preferences_snapshot(state, &mut snapshot).await;
    fan_out_agents_view_preferences(state, snapshot).await;
}

async fn fan_out_session_schemas(state: &mut HostState, force_emit: bool) {
    let enabled_backends = state
        .settings_store
        .lock()
        .await
        .get()
        .unwrap_or_else(|err| panic!("failed to load host settings for session schemas: {err}"))
        .enabled_backends;
    let schemas = session_schemas_for_enabled_backends(state, &enabled_backends);
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_session_schemas_for_subscriber(&schemas, subscriber, force_emit)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

#[derive(Default)]
struct BackendSettingsSnapshots {
    backend_config: Vec<BackendConfigSnapshot>,
    native_settings: Vec<BackendNativeSettingsSnapshot>,
}

async fn backend_config_snapshots_for_enabled_backends(
    enabled_backends: &[protocol::BackendKind],
) -> BackendSettingsSnapshots {
    let workspace_roots = match hermes_probe_workspace_root() {
        Ok(root) => vec![root],
        Err(err) => {
            tracing::error!(
                "failed to resolve backend config snapshot probe workspace root: {err}"
            );
            Vec::new()
        }
    };
    let mut snapshots = BackendSettingsSnapshots::default();
    for kind in enabled_backends {
        match kind {
            BackendKind::Tycode => {
                snapshots
                    .native_settings
                    .push(crate::backend::tycode::native_settings_snapshot().await);
            }
            BackendKind::Hermes
            | BackendKind::Kiro
            | BackendKind::Claude
            | BackendKind::Codex
            | BackendKind::Antigravity => {}
        }
    }
    // The Hermes snapshot is published regardless of enablement, like the
    // deep-config schema catalog: its settings page edits Hermes's own
    // config, which users legitimately configure before enabling the
    // backend. An uninstalled Hermes yields a visible Unavailable snapshot.
    snapshots
        .native_settings
        .push(crate::backend::hermes::native_settings_snapshot(&workspace_roots).await);
    snapshots
}

async fn fan_out_backend_config_schemas(state: &mut HostState) {
    let schemas = crate::backend::backend_config_schema_catalog();
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_backend_config_schemas_for_subscriber(&schemas, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn fan_out_backend_config_snapshots(state: &mut HostState, force_emit: bool) {
    let snapshots = state.backend_config_snapshots.clone();
    let native_settings = state.backend_native_settings_snapshots.clone();
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_backend_config_snapshots_for_subscriber(
            &snapshots,
            &native_settings,
            subscriber,
            force_emit,
        )
        .await
        .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

fn initial_backend_capacity_snapshots() -> HashMap<BackendKind, BackendCapacitySnapshot> {
    const BACKENDS: [BackendKind; 6] = [
        BackendKind::Tycode,
        BackendKind::Kiro,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Hermes,
    ];
    BACKENDS
        .into_iter()
        .map(|backend_kind| {
            let state = match backend_kind {
                BackendKind::Claude | BackendKind::Codex => BackendCapacityState::Unavailable {
                    reason: protocol::CapacityUnavailableReason::AwaitingFirstReport,
                },
                _ => BackendCapacityState::Unsupported {
                    reason: protocol::CapacityUnsupportedReason::BackendHasNoCapacitySource,
                },
            };
            (
                backend_kind,
                BackendCapacitySnapshot {
                    backend_kind,
                    state,
                    retrieved_at_ms: capacity_now_ms(),
                    freshness: protocol::CapacityFreshness::Fresh { age_ms: 0 },
                },
            )
        })
        .collect()
}

fn backend_capacity_snapshots(state: &HostState) -> Vec<BackendCapacitySnapshot> {
    let now = capacity_now_ms();
    let mut snapshots = state
        .backend_capacity
        .values()
        .cloned()
        .map(|mut snapshot| {
            let age_ms = now.saturating_sub(snapshot.retrieved_at_ms);
            match &snapshot.state {
                BackendCapacityState::Known { report } if age_ms >= 60 * 60 * 1000 => {
                    snapshot.state = BackendCapacityState::Stale {
                        report: report.clone(),
                        stale_since_ms: snapshot.retrieved_at_ms.saturating_add(60 * 60 * 1000),
                    };
                    snapshot.freshness = protocol::CapacityFreshness::Stale {
                        age_ms,
                        threshold_ms: 60 * 60 * 1000,
                    };
                }
                BackendCapacityState::Known { .. } => {
                    snapshot.freshness = protocol::CapacityFreshness::Fresh { age_ms };
                }
                BackendCapacityState::Stale { .. } => {
                    snapshot.freshness = protocol::CapacityFreshness::Stale {
                        age_ms,
                        threshold_ms: 60 * 60 * 1000,
                    };
                }
                _ => snapshot.freshness = protocol::CapacityFreshness::Fresh { age_ms },
            }
            snapshot
        })
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|snapshot| match snapshot.backend_kind {
        BackendKind::Tycode => 0,
        BackendKind::Kiro => 1,
        BackendKind::Claude => 2,
        BackendKind::Codex => 3,
        BackendKind::Antigravity => 4,
        BackendKind::Hermes => 5,
    });
    snapshots
}

fn fan_out_backend_capacity(state: &mut HostState) {
    let snapshots = backend_capacity_snapshots(state);
    let paths = state.host_streams.keys().cloned().collect::<Vec<_>>();
    let mut dead_paths = Vec::new();
    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if !subscriber.capacity_replay_ready {
            continue;
        }
        if emit_backend_capacity_for_subscriber(&snapshots, subscriber).is_err() {
            dead_paths.push(path);
        }
    }
    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

fn emit_backend_capacity_for_subscriber(
    snapshots: &[BackendCapacitySnapshot],
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    if subscriber.last_backend_capacity.as_deref() == Some(snapshots) {
        return Ok(());
    }
    let payload = serde_json::to_value(BackendCapacityPayload {
        snapshots: snapshots.to_vec(),
    })
    .expect("failed to serialize BackendCapacity payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::BackendCapacity, payload)?;
    subscriber.last_backend_capacity = Some(snapshots.to_vec());
    Ok(())
}

fn capacity_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn fan_out_launch_profile_catalog(state: &mut HostState) {
    let settings = state
        .settings_store
        .lock()
        .await
        .get()
        .unwrap_or_else(|err| {
            panic!("failed to load host settings for launch profile catalog: {err}")
        });
    let catalog = launch_profile_catalog_for_settings(state, &settings);
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_launch_profile_catalog_for_subscriber(&catalog, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn emit_backend_config_schemas_for_subscriber(
    schemas: &[protocol::BackendConfigSchema],
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    if subscriber.last_backend_config_schemas.as_deref() == Some(schemas) {
        return Ok(());
    }
    let payload = serde_json::to_value(protocol::BackendConfigSchemasPayload {
        schemas: schemas.to_vec(),
    })
    .expect("failed to serialize BackendConfigSchemas payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::BackendConfigSchemas, payload)?;
    subscriber.last_backend_config_schemas = Some(schemas.to_vec());
    Ok(())
}

async fn emit_backend_config_snapshots_for_subscriber(
    snapshots: &[BackendConfigSnapshot],
    native_settings: &[BackendNativeSettingsSnapshot],
    subscriber: &mut HostSubscriber,
    force_emit: bool,
) -> Result<(), StreamClosed> {
    if !force_emit
        && subscriber.last_backend_config_snapshots.as_deref() == Some(snapshots)
        && subscriber.last_backend_native_settings_snapshots.as_deref() == Some(native_settings)
    {
        return Ok(());
    }
    let payload = serde_json::to_value(BackendConfigSnapshotsPayload {
        snapshots: snapshots.to_vec(),
        native_settings: native_settings.to_vec(),
    })
    .expect("failed to serialize BackendConfigSnapshots payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::BackendConfigSnapshots, payload)?;
    subscriber.last_backend_config_snapshots = Some(snapshots.to_vec());
    subscriber.last_backend_native_settings_snapshots = Some(native_settings.to_vec());
    Ok(())
}

async fn emit_launch_profile_catalog_for_subscriber(
    catalog: &LaunchProfileCatalog,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    if subscriber.last_launch_profile_catalog.as_ref() == Some(catalog) {
        return Ok(());
    }
    let payload = serde_json::to_value(LaunchProfileCatalogPayload {
        catalog: catalog.clone(),
    })
    .expect("failed to serialize LaunchProfileCatalog payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::LaunchProfileCatalogNotify, payload)?;
    subscriber.last_launch_profile_catalog = Some(catalog.clone());
    Ok(())
}

async fn fan_out_backend_setup(state: &mut HostState, payload: BackendSetupPayload) {
    let paths: Vec<StreamPath> = state.host_streams.keys().cloned().collect();
    let mut dead_paths = Vec::new();

    for path in paths {
        let Some(subscriber) = state.host_streams.get_mut(&path) else {
            continue;
        };
        if emit_backend_setup_for_subscriber(&payload, subscriber)
            .await
            .is_err()
        {
            dead_paths.push(path);
        }
    }

    for path in dead_paths {
        state.host_streams.remove(&path);
    }
}

async fn emit_team_notify_for_subscriber(
    payload: &TeamNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::TeamNotify, payload)
}

async fn emit_team_member_notify_for_subscriber(
    payload: &TeamMemberNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamMemberNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::TeamMemberNotify, payload)
}

async fn emit_team_member_binding_notify_for_subscriber(
    payload: &TeamMemberBindingNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamMemberBindingNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::TeamMemberBindingNotify, payload)
}

async fn emit_team_draft_notify_for_subscriber(
    payload: &TeamDraftNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TeamDraftNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::TeamDraftNotify, payload)
}

async fn emit_team_member_shuffle_suggestion_for_subscriber(
    payload: &TeamMemberShuffleSuggestionNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload).expect(
        "failed to serialize TeamMemberShuffleSuggestionNotify payload for host stream fanout",
    );
    emit_or_queue_host_frame(
        subscriber,
        FrameKind::TeamMemberShuffleSuggestionNotify,
        payload,
    )
}

async fn emit_project_notify_for_subscriber(
    payload: &ProjectNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize ProjectNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::ProjectNotify, payload)
}

async fn emit_custom_agent_notify_for_subscriber(
    payload: &CustomAgentNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize CustomAgentNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::CustomAgentNotify, payload)
}

async fn emit_steering_notify_for_subscriber(
    payload: &SteeringNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize SteeringNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::SteeringNotify, payload)
}

async fn emit_skill_notify_for_subscriber(
    payload: &SkillNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize SkillNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::SkillNotify, payload)
}

async fn emit_mcp_server_notify_for_subscriber(
    payload: &McpServerNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize McpServerNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::McpServerNotify, payload)
}

async fn emit_workflow_notify_for_subscriber(
    payload: &WorkflowNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize WorkflowNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::WorkflowNotify, payload)
}

async fn emit_workflow_run_notify_for_subscriber(
    payload: &WorkflowRunNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize WorkflowRunNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::WorkflowRunNotify, payload)
}

async fn emit_host_settings_for_subscriber(
    settings: &protocol::HostSettings,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(HostSettingsPayload {
        settings: settings.clone(),
    })
    .expect("failed to serialize HostSettings payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::HostSettings, payload)
}

async fn emit_agent_activity_summary_for_subscriber(
    payload: &AgentActivitySummaryPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize AgentActivitySummary payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::AgentActivitySummary, payload)
}

async fn emit_task_token_usage_for_subscriber(
    payload: &TaskTokenUsagePayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize TaskTokenUsage payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::TaskTokenUsage, payload)
}

async fn emit_agents_view_preferences_for_subscriber(
    payload: &AgentsViewPreferencesNotifyPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize AgentsViewPreferencesNotify payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::AgentsViewPreferencesNotify, payload)
}

async fn emit_backend_setup_for_subscriber(
    payload: &BackendSetupPayload,
    subscriber: &mut HostSubscriber,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize BackendSetup payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::BackendSetup, payload)
}

async fn emit_agent_closed_for_stream(
    payload: &AgentClosedPayload,
    stream: &Stream,
) -> Result<(), StreamClosed> {
    let payload = serde_json::to_value(payload)
        .expect("failed to serialize AgentClosed payload for host stream fanout");
    stream.send_value(FrameKind::AgentClosed, payload)
}

async fn emit_session_schemas_for_subscriber(
    schemas: &[SessionSchemaEntry],
    subscriber: &mut HostSubscriber,
    force_emit: bool,
) -> Result<(), StreamClosed> {
    if !force_emit && subscriber.last_session_schemas.as_deref() == Some(schemas) {
        return Ok(());
    }
    let payload = serde_json::to_value(SessionSchemasPayload {
        schemas: schemas.to_vec(),
    })
    .expect("failed to serialize SessionSchemas payload for host stream fanout");
    emit_or_queue_host_frame(subscriber, FrameKind::SessionSchemas, payload)?;
    subscriber.last_session_schemas = Some(schemas.to_vec());
    Ok(())
}

/// The persisted deep-config values to apply for a spawn of `backend_kind`,
/// re-sanitized against the current schema. Empty when unconfigured.
fn resolve_backend_config_for_spawn(
    host_settings: &protocol::HostSettings,
    backend_kind: protocol::BackendKind,
) -> protocol::BackendConfigValues {
    host_settings
        .backend_config
        .get(&backend_kind)
        .map(|values| crate::backend::sanitize_backend_config_values(backend_kind, values))
        .unwrap_or_default()
}

fn session_schema_for_backend(
    state: &HostState,
    backend_kind: protocol::BackendKind,
) -> Option<SessionSettingsSchema> {
    match backend_kind {
        protocol::BackendKind::Codex => match &state.codex_session_schema {
            CodexSessionSchemaState::Ready(schema) => Some(schema.clone()),
            CodexSessionSchemaState::Pending | CodexSessionSchemaState::Unavailable(_) => None,
        },
        protocol::BackendKind::Kiro => match &state.kiro_session_schema {
            KiroSessionSchemaState::Ready(schema) => Some(schema.clone()),
            KiroSessionSchemaState::Pending | KiroSessionSchemaState::Unavailable(_) => None,
        },
        protocol::BackendKind::Hermes => match &state.hermes_session_schema {
            HermesSessionSchemaState::Ready(schema) => Some(schema.clone()),
            HermesSessionSchemaState::Pending | HermesSessionSchemaState::Unavailable(_) => None,
        },
        _ => Some(session_settings_schema_for_backend(backend_kind)),
    }
}

fn session_schema_resolution_for_backend(
    state: &HostState,
    backend_kind: protocol::BackendKind,
) -> SessionSchemaResolution {
    match backend_kind {
        protocol::BackendKind::Codex => match &state.codex_session_schema {
            CodexSessionSchemaState::Pending => SessionSchemaResolution::Pending,
            CodexSessionSchemaState::Ready(schema) => {
                SessionSchemaResolution::Ready(schema.clone())
            }
            CodexSessionSchemaState::Unavailable(message) => {
                SessionSchemaResolution::Unavailable(message.clone())
            }
        },
        protocol::BackendKind::Kiro => match &state.kiro_session_schema {
            KiroSessionSchemaState::Pending => SessionSchemaResolution::Pending,
            KiroSessionSchemaState::Ready(schema) => SessionSchemaResolution::Ready(schema.clone()),
            KiroSessionSchemaState::Unavailable(message) => {
                SessionSchemaResolution::Unavailable(message.clone())
            }
        },
        protocol::BackendKind::Hermes => match &state.hermes_session_schema {
            HermesSessionSchemaState::Pending => SessionSchemaResolution::Pending,
            HermesSessionSchemaState::Ready(schema) => {
                SessionSchemaResolution::Ready(schema.clone())
            }
            HermesSessionSchemaState::Unavailable(message) => {
                SessionSchemaResolution::Unavailable(message.clone())
            }
        },
        _ => SessionSchemaResolution::Ready(session_settings_schema_for_backend(backend_kind)),
    }
}

fn tier_values_for_hint(
    hint: protocol::SpawnCostHint,
    config: &protocol::BackendTierConfig,
) -> protocol::SessionSettingsValues {
    match hint {
        protocol::SpawnCostHint::Low => config.low.clone(),
        protocol::SpawnCostHint::Medium => protocol::SessionSettingsValues::default(),
        protocol::SpawnCostHint::High => config.high.clone(),
    }
}

fn session_settings_startup_failure(
    backend_kind: protocol::BackendKind,
    schema: Option<&SessionSettingsSchema>,
    settings: &protocol::SessionSettingsValues,
    source: &str,
) -> Option<AgentStartupFailure> {
    if settings.0.is_empty() {
        return None;
    }
    let Some(schema) = schema else {
        return Some(AgentStartupFailure::backend_failed(format!(
            "{backend_kind:?} session settings schema unavailable; cannot apply {source} session settings"
        )));
    };
    validate_session_settings_values(schema, settings)
        .err()
        .map(|err| {
            AgentStartupFailure::internal(format!(
                "invalid {source} session settings for backend {backend_kind:?}: {err}"
            ))
        })
}

fn sanitize_stored_session_settings(
    backend_kind: protocol::BackendKind,
    schema: Option<&SessionSettingsSchema>,
    stored_settings: Option<protocol::SessionSettingsValues>,
) -> (
    Option<protocol::SessionSettingsValues>,
    Option<AgentStartupFailure>,
) {
    let Some(stored_settings) = stored_settings else {
        return (None, None);
    };
    if stored_settings.0.is_empty() {
        return (Some(stored_settings), None);
    }
    let Some(schema) = schema else {
        return (
            None,
            Some(AgentStartupFailure::backend_failed(format!(
                "{backend_kind:?} session settings schema unavailable; cannot apply stored session settings"
            ))),
        );
    };
    if let Err(err) = validate_session_settings_values(schema, &stored_settings) {
        return (
            None,
            Some(AgentStartupFailure::internal(format!(
                "invalid stored session settings for backend {backend_kind:?}: {err}"
            ))),
        );
    }
    (
        Some(sanitize_session_settings_values(schema, &stored_settings)),
        None,
    )
}

fn backend_has_dynamic_session_schema(backend_kind: protocol::BackendKind) -> bool {
    matches!(
        backend_kind,
        protocol::BackendKind::Kiro | protocol::BackendKind::Codex | protocol::BackendKind::Hermes
    )
}

fn session_schema_entry_for_backend(
    state: &HostState,
    backend_kind: protocol::BackendKind,
) -> SessionSchemaEntry {
    match backend_kind {
        protocol::BackendKind::Codex => match &state.codex_session_schema {
            CodexSessionSchemaState::Ready(schema) => SessionSchemaEntry::Ready {
                schema: schema.clone(),
            },
            CodexSessionSchemaState::Pending => SessionSchemaEntry::Pending { backend_kind },
            CodexSessionSchemaState::Unavailable(message) => SessionSchemaEntry::Unavailable {
                backend_kind,
                message: message.clone(),
            },
        },
        protocol::BackendKind::Kiro => match &state.kiro_session_schema {
            KiroSessionSchemaState::Ready(schema) => SessionSchemaEntry::Ready {
                schema: schema.clone(),
            },
            KiroSessionSchemaState::Pending => SessionSchemaEntry::Pending { backend_kind },
            KiroSessionSchemaState::Unavailable(message) => SessionSchemaEntry::Unavailable {
                backend_kind,
                message: message.clone(),
            },
        },
        protocol::BackendKind::Hermes => match &state.hermes_session_schema {
            HermesSessionSchemaState::Ready(schema) => SessionSchemaEntry::Ready {
                schema: schema.clone(),
            },
            HermesSessionSchemaState::Pending => SessionSchemaEntry::Pending { backend_kind },
            HermesSessionSchemaState::Unavailable(message) => SessionSchemaEntry::Unavailable {
                backend_kind,
                message: message.clone(),
            },
        },
        _ => SessionSchemaEntry::Ready {
            schema: session_settings_schema_for_backend(backend_kind),
        },
    }
}

fn session_schemas_for_enabled_backends(
    state: &HostState,
    enabled_backends: &[protocol::BackendKind],
) -> Vec<SessionSchemaEntry> {
    enabled_backends
        .iter()
        .copied()
        .map(|backend_kind| session_schema_entry_for_backend(state, backend_kind))
        .collect()
}

fn launch_profile_catalog_for_settings(
    state: &HostState,
    settings: &protocol::HostSettings,
) -> LaunchProfileCatalog {
    let mut entries = Vec::new();
    for backend_kind in settings.enabled_backends.iter().copied() {
        entries.push(LaunchProfileEntry::Ready {
            profile: LaunchProfile {
                id: default_launch_profile_id(backend_kind),
                kind: LaunchProfileKind::BackendDefault,
                label: backend_launch_profile_label(backend_kind).to_owned(),
                description: Some(format!(
                    "Launch {} with its backend defaults.",
                    backend_launch_profile_label(backend_kind)
                )),
                backend_kind,
                session_settings: protocol::SessionSettingsValues::default(),
            },
        });
    }

    // One synthesized entry per named Hermes profile, derived from the last
    // schema probe's discovery. The default profile is already covered by the
    // "hermes:default" backend-default entry above.
    if settings
        .enabled_backends
        .contains(&protocol::BackendKind::Hermes)
    {
        synthesize_hermes_profile_entries(&state.hermes_launch_profiles, &mut entries);
    }

    for config in &settings.launch_profiles {
        if settings.enabled_backends.contains(&config.backend_kind) {
            entries.push(launch_profile_entry_for_config(state, config));
        }
    }

    LaunchProfileCatalog {
        entries,
        default_profile_id: settings.default_backend.map(default_launch_profile_id),
    }
}

/// Id namespace for launch profiles synthesized from Hermes profiles. Also
/// reserved against user-configured launch-profile ids.
pub(crate) const HERMES_PROFILE_LAUNCH_ID_PREFIX: &str = "hermes:profile:";

/// Append one launch-profile entry per named Hermes profile: ready entries
/// carry `{profile: <name>}` session settings; profiles whose gateway probe
/// failed stay visible as unavailable entries with the probe error.
fn synthesize_hermes_profile_entries(
    infos: &[crate::backend::hermes::HermesLaunchProfileInfo],
    entries: &mut Vec<LaunchProfileEntry>,
) {
    for info in infos {
        if info.name == protocol::hermes_config::HERMES_DEFAULT_PROFILE {
            continue;
        }
        let id = LaunchProfileId(format!("{HERMES_PROFILE_LAUNCH_ID_PREFIX}{}", info.name));
        let label = format!("Hermes — {}", info.name);
        match &info.error {
            None => {
                let mut session_settings = protocol::SessionSettingsValues::default();
                session_settings.0.insert(
                    crate::backend::hermes::HERMES_PROFILE_SETTING.to_owned(),
                    protocol::SessionSettingValue::String(info.name.clone()),
                );
                entries.push(LaunchProfileEntry::Ready {
                    profile: LaunchProfile {
                        id,
                        kind: LaunchProfileKind::BackendDefault,
                        label,
                        description: Some(match &info.summary {
                            Some(summary) => format!(
                                "Launch Hermes with its '{}' profile ({summary}).",
                                info.name
                            ),
                            None => format!("Launch Hermes with its '{}' profile.", info.name),
                        }),
                        backend_kind: protocol::BackendKind::Hermes,
                        session_settings,
                    },
                });
            }
            Some(error) => entries.push(LaunchProfileEntry::Unavailable {
                id,
                kind: LaunchProfileKind::BackendDefault,
                backend_kind: protocol::BackendKind::Hermes,
                label,
                message: error.clone(),
            }),
        }
    }
}

fn default_launch_profile_id(backend_kind: protocol::BackendKind) -> LaunchProfileId {
    LaunchProfileId(format!("{}:default", backend_slug(backend_kind)))
}

fn backend_slug(backend_kind: protocol::BackendKind) -> &'static str {
    match backend_kind {
        protocol::BackendKind::Tycode => "tycode",
        protocol::BackendKind::Kiro => "kiro",
        protocol::BackendKind::Claude => "claude",
        protocol::BackendKind::Codex => "codex",
        protocol::BackendKind::Antigravity => "antigravity",
        protocol::BackendKind::Hermes => "hermes",
    }
}

fn backend_launch_profile_label(backend_kind: protocol::BackendKind) -> &'static str {
    match backend_kind {
        protocol::BackendKind::Tycode => "Tycode",
        protocol::BackendKind::Kiro => "Kiro",
        protocol::BackendKind::Claude => "Claude",
        protocol::BackendKind::Codex => "Codex",
        protocol::BackendKind::Antigravity => "Antigravity",
        protocol::BackendKind::Hermes => "Hermes",
    }
}

fn launch_profile_entry_for_config(
    state: &HostState,
    config: &HostLaunchProfileConfig,
) -> LaunchProfileEntry {
    if config.session_settings.0.is_empty() {
        return LaunchProfileEntry::Ready {
            profile: LaunchProfile {
                id: config.id.clone(),
                kind: LaunchProfileKind::Custom,
                label: config.label.clone(),
                description: config.description.clone(),
                backend_kind: config.backend_kind,
                session_settings: protocol::SessionSettingsValues::default(),
            },
        };
    }

    match session_schema_entry_for_backend(state, config.backend_kind) {
        SessionSchemaEntry::Ready { schema } => {
            match validate_session_settings_values(&schema, &config.session_settings) {
                Ok(()) => LaunchProfileEntry::Ready {
                    profile: LaunchProfile {
                        id: config.id.clone(),
                        kind: LaunchProfileKind::Custom,
                        label: config.label.clone(),
                        description: config.description.clone(),
                        backend_kind: config.backend_kind,
                        session_settings: config.session_settings.clone(),
                    },
                },
                Err(error) => LaunchProfileEntry::Unavailable {
                    id: config.id.clone(),
                    kind: LaunchProfileKind::Custom,
                    backend_kind: config.backend_kind,
                    label: config.label.clone(),
                    message: format!(
                        "configured launch profile session settings are invalid: {error}"
                    ),
                },
            }
        }
        SessionSchemaEntry::Pending { .. } => LaunchProfileEntry::Unavailable {
            id: config.id.clone(),
            kind: LaunchProfileKind::Custom,
            backend_kind: config.backend_kind,
            label: config.label.clone(),
            message: format!(
                "{:?} session settings schema is still loading; launch profile is not available yet",
                config.backend_kind
            ),
        },
        SessionSchemaEntry::Unavailable { message, .. } => LaunchProfileEntry::Unavailable {
            id: config.id.clone(),
            kind: LaunchProfileKind::Custom,
            backend_kind: config.backend_kind,
            label: config.label.clone(),
            message: format!(
                "{:?} session settings schema is unavailable: {message}",
                config.backend_kind
            ),
        },
    }
}

fn resolve_launch_profile_from_catalog(
    catalog: &LaunchProfileCatalog,
    launch_profile_id: &LaunchProfileId,
) -> Result<LaunchProfile, String> {
    let Some(entry) = catalog
        .entries
        .iter()
        .find(|entry| entry.id() == launch_profile_id)
    else {
        return Err(format!("unknown launch_profile_id {launch_profile_id}"));
    };
    match entry {
        LaunchProfileEntry::Ready { profile } => Ok(profile.clone()),
        LaunchProfileEntry::Unavailable { message, .. } => Err(format!(
            "launch_profile_id {launch_profile_id} is unavailable: {message}"
        )),
    }
}

fn kiro_probe_workspace_root(configured_root: Option<&Path>) -> Result<String, String> {
    match configured_root {
        Some(root) => Ok(root.to_string_lossy().into_owned()),
        None => Ok(crate::paths::home_dir()?.to_string_lossy().into_owned()),
    }
}

fn hermes_probe_workspace_root() -> Result<String, String> {
    Ok(crate::paths::home_dir()?.to_string_lossy().into_owned())
}

fn new_instance_stream(agent_id: &AgentId) -> StreamPath {
    let instance_id = Uuid::new_v4();
    StreamPath(format!("/agent/{}/{}", agent_id, instance_id))
}

fn default_compaction_summary_prompt() -> String {
    r#"You are writing a handoff note for the Tyde agent that will replace you after this session is compacted. It will see only this note — not the conversation. Capture the durable context it needs to continue seamlessly, and nothing else.

If there is genuinely nothing durable to carry forward, output exactly `No durable context.` and stop.

Otherwise use these sections; omit any that are empty rather than padding them:

- **Objective & acceptance criteria** — what we are ultimately trying to achieve and what "done" looks like.
- **Current state & next step** — what is in progress now and the single most important next action.
- **Open threads** — unfinished work, unanswered questions, and known blockers.
- **Decisions, rationale & rejected alternatives** — what was decided or ruled out and why, so the replacement neither relitigates nor violates them.
- **Key paths, artifacts & how to verify** — the specific files, commands, endpoints, and checks needed to continue and to confirm work is correct.
- **Project & environment facts** — durable, non-obvious facts about the code, tools, and setup.
- **User preferences** — how this user wants work done.
- **Learnings & dead-ends** — insights worth keeping, and failed attempts **only** where knowing they failed constrains future work.
- **Uncertainty & open disagreements** — what is unverified or unresolved.

Rules: Record only what remains true and useful for future work; drop transient chatter, resolved dead-ends, and step-by-step narration. Preserve specifics — exact names, paths, commands, values, and error signatures — over vague description. Mark anything unverified as an assumption; never invent facts, decisions, or outcomes you cannot support from this session, and say plainly when something important is unknown. **Never include secrets, tokens, keys, or credentials.** Be concise: prefer the shortest form a replacement could act on without re-deriving it. Output only the note (or the `No durable context.` sentinel)."#
        .to_owned()
}

fn build_compaction_replacement_prompt(summary: &str) -> String {
    format!(
        "You are replacing a previous persistent Tyde agent after session compaction. \
Use this memory summary as your durable context, but do not claim you can access the \
old session transcript unless the user provides it.\n\nMemory summary:\n{summary}\n\n\
Acknowledge briefly and continue from this memory."
    )
}

fn compaction_summary_preview(summary: &str) -> String {
    summary
        .chars()
        .take(COMPACTION_SUMMARY_PREVIEW_CHARS)
        .collect()
}

fn send_agent_compact_notify(stream: &Stream, payload: AgentCompactNotifyPayload) {
    let value = serde_json::to_value(payload)
        .expect("failed to serialize AgentCompactNotify payload for agent stream");
    let _ = stream.send_value(FrameKind::AgentCompactNotify, value);
}

fn drain_final_agent_compact_notify(
    old_agent_id: &AgentId,
    rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
) -> AgentCompactNotifyPayload {
    let mut final_notify = None;
    while let Ok(envelope) = rx.try_recv() {
        if envelope.kind != FrameKind::AgentCompactNotify {
            continue;
        }
        match envelope.parse_payload::<AgentCompactNotifyPayload>() {
            Ok(payload)
                if matches!(
                    payload.status,
                    AgentCompactStatus::Completed | AgentCompactStatus::Failed
                ) =>
            {
                final_notify = Some(payload);
            }
            Ok(_) => {}
            Err(error) => {
                return AgentCompactNotifyPayload {
                    status: AgentCompactStatus::Failed,
                    old_agent_id: old_agent_id.clone(),
                    old_session_id: None,
                    new_agent_id: None,
                    new_session_id: None,
                    summary_preview: None,
                    message: Some(format!("failed to parse compaction result: {error}")),
                };
            }
        }
    }

    final_notify.unwrap_or(AgentCompactNotifyPayload {
        status: AgentCompactStatus::Failed,
        old_agent_id: old_agent_id.clone(),
        old_session_id: None,
        new_agent_id: None,
        new_session_id: None,
        summary_preview: None,
        message: Some("agent compaction finished without a terminal notify".to_owned()),
    })
}

fn send_team_compact_notify(stream: &Stream, payload: TeamCompactNotifyPayload) {
    let value = serde_json::to_value(payload)
        .expect("failed to serialize TeamCompactNotify payload for host stream");
    let _ = stream.send_value(FrameKind::TeamCompactNotify, value);
}

impl HostHandle {
    /// Record a passive backend capacity update. The host is the sole owner of
    /// this account-wide state; agent connections never retain capacity state.
    pub(crate) async fn record_backend_capacity(
        &self,
        backend_kind: BackendKind,
        state: BackendCapacityState,
    ) {
        let retrieved_at_ms = capacity_now_ms();
        let snapshot = BackendCapacitySnapshot {
            backend_kind,
            state,
            retrieved_at_ms,
            freshness: protocol::CapacityFreshness::Fresh { age_ms: 0 },
        };
        let repeated = {
            let mut host_state = self.state.lock().await;
            if host_state
                .backend_capacity
                .get(&backend_kind)
                .is_some_and(|current| current.state == snapshot.state)
            {
                let current = host_state
                    .backend_capacity
                    .get_mut(&backend_kind)
                    .expect("checked capacity snapshot must exist");
                current.retrieved_at_ms = retrieved_at_ms;
                current.freshness = protocol::CapacityFreshness::Fresh { age_ms: 0 };
                true
            } else {
                host_state.backend_capacity.insert(backend_kind, snapshot);
                fan_out_backend_capacity(&mut host_state);
                false
            }
        };
        let host = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
            host.mark_backend_capacity_stale(backend_kind, retrieved_at_ms)
                .await;
        });
        if repeated {
            tracing::debug!(?backend_kind, "refreshed identical passive capacity report");
        }
    }

    async fn mark_backend_capacity_stale(&self, backend_kind: BackendKind, retrieved_at_ms: u64) {
        let mut state = self.state.lock().await;
        let now = capacity_now_ms();
        let Some(snapshot) = state.backend_capacity.get_mut(&backend_kind) else {
            return;
        };
        if snapshot.retrieved_at_ms != retrieved_at_ms {
            return;
        }
        let BackendCapacityState::Known { report } = &snapshot.state else {
            return;
        };
        snapshot.state = BackendCapacityState::Stale {
            report: report.clone(),
            stale_since_ms: now,
        };
        snapshot.freshness = protocol::CapacityFreshness::Stale {
            age_ms: now.saturating_sub(retrieved_at_ms),
            threshold_ms: 60 * 60 * 1000,
        };
        fan_out_backend_capacity(&mut state);
    }
    async fn refresh_after_project_mutation(
        &self,
        connection_host_stream: &StreamPath,
        project_output_stream: Stream,
        project_id: ProjectId,
        _path: Option<ProjectPath>,
    ) -> AppResult<()> {
        let handle = self
            .ensure_host_project_subscription(
                connection_host_stream,
                &project_output_stream,
                project_id,
                "project_mutation_refresh",
            )
            .await?;
        handle
            .refresh()
            .await
            .map_err(|error| project_command_error("project_mutation_refresh", error))?;
        Ok(())
    }

    async fn terminal_handle(
        &self,
        connection_host_stream: &StreamPath,
        terminal_id: &TerminalId,
    ) -> AppResult<TerminalHandle> {
        let state = self.state.lock().await;
        state
            .terminal_streams
            .get(&(connection_host_stream.clone(), terminal_id.clone()))
            .cloned()
            .ok_or_else(|| {
                AppError::not_found(
                    "terminal_lookup",
                    format!(
                        "terminal {} is not owned by host stream {}",
                        terminal_id, connection_host_stream
                    ),
                )
            })
    }
}

async fn resolve_terminal_launch(
    project_store: &Arc<Mutex<ProjectStore>>,
    payload: TerminalCreatePayload,
) -> AppResult<TerminalLaunchInfo> {
    const OPERATION: &str = "terminal_create";
    match payload.target {
        TerminalLaunchTarget::HostDefault => {
            let cwd = std::env::current_dir()
                .context("failed to resolve host default cwd")
                .map_err(|error| AppError::internal(OPERATION, error))?
                .display()
                .to_string();
            Ok(TerminalLaunchInfo {
                project_id: None,
                root: None,
                cwd,
                cols: payload.cols,
                rows: payload.rows,
                command: TerminalLaunchCommand::DefaultShell,
            })
        }
        TerminalLaunchTarget::Project {
            project_id,
            root,
            relative_cwd,
        } => {
            let project = load_project(project_store, &project_id, OPERATION).await?;
            let roots = project.root_paths().into_iter().collect::<HashSet<_>>();
            if !roots.contains(&root) {
                return Err(AppError::invalid(
                    OPERATION,
                    format!(
                        "cannot create terminal in root {} that is not part of project {}",
                        root, project_id
                    ),
                ));
            }

            let cwd = resolve_project_terminal_cwd(&root, relative_cwd.as_deref())
                .map_err(|error| AppError::invalid(OPERATION, error))?;
            Ok(TerminalLaunchInfo {
                project_id: Some(project_id),
                root: Some(root),
                cwd,
                cols: payload.cols,
                rows: payload.rows,
                command: TerminalLaunchCommand::DefaultShell,
            })
        }
        TerminalLaunchTarget::Path { cwd } => {
            let trimmed = cwd.trim();
            if trimmed.is_empty() {
                return Err(AppError::invalid(
                    OPERATION,
                    "terminal path cwd must not be empty",
                ));
            }
            if !Path::new(trimmed).is_absolute() {
                return Err(AppError::invalid(
                    OPERATION,
                    format!("terminal path cwd must be absolute: {}", trimmed),
                ));
            }
            Ok(TerminalLaunchInfo {
                project_id: None,
                root: None,
                cwd: trimmed.to_owned(),
                cols: payload.cols,
                rows: payload.rows,
                command: TerminalLaunchCommand::DefaultShell,
            })
        }
    }
}

fn resolve_project_terminal_cwd(
    root: &ProjectRootPath,
    relative_cwd: Option<&str>,
) -> Result<String, String> {
    let Some(relative_cwd) = relative_cwd else {
        return Ok(root.0.clone());
    };
    validate_terminal_relative_path(relative_cwd)?;
    Ok(Path::new(&root.0)
        .join(relative_cwd)
        .to_string_lossy()
        .to_string())
}

fn validate_terminal_relative_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("terminal relative_cwd must not be empty".to_owned());
    }

    let relative = Path::new(path);
    if !relative.is_relative() {
        return Err(format!("terminal relative_cwd must be relative: {}", path));
    }

    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "terminal relative_cwd must not contain '..': {}",
                    path
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("terminal relative_cwd must be relative: {}", path));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::{MOCK_DIE_AFTER_BUSY_SENTINEL, MOCK_SLOW_TURN_SENTINEL};
    use crate::review::ReviewHandle;
    use crate::store::agent_teams::AgentTeamsStoreFile;
    use protocol::{
        AgentErrorPayload, BackendConfigSnapshotStatus, BackendConfigSnapshotsPayload, BackendKind,
        BackendNativeSettingsSnapshot, CustomAgentId, DiffContextMode, HostSettingValue,
        ProjectDiffScope, ProjectGitDiffFile, ProjectGitDiffPayload, ProtocolValidator, Review,
        ReviewAiReviewerState, ReviewAiReviewerStatus, ReviewStatus, TeamMemberCreateSpec,
        ToolPolicy,
    };

    static STARTUP_FAILURE_FANOUT_RACE_TEST_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    #[test]
    fn default_compaction_prompt_matches_approved_handoff_note() {
        let expected = r#"You are writing a handoff note for the Tyde agent that will replace you after this session is compacted. It will see only this note — not the conversation. Capture the durable context it needs to continue seamlessly, and nothing else.

If there is genuinely nothing durable to carry forward, output exactly `No durable context.` and stop.

Otherwise use these sections; omit any that are empty rather than padding them:

- **Objective & acceptance criteria** — what we are ultimately trying to achieve and what "done" looks like.
- **Current state & next step** — what is in progress now and the single most important next action.
- **Open threads** — unfinished work, unanswered questions, and known blockers.
- **Decisions, rationale & rejected alternatives** — what was decided or ruled out and why, so the replacement neither relitigates nor violates them.
- **Key paths, artifacts & how to verify** — the specific files, commands, endpoints, and checks needed to continue and to confirm work is correct.
- **Project & environment facts** — durable, non-obvious facts about the code, tools, and setup.
- **User preferences** — how this user wants work done.
- **Learnings & dead-ends** — insights worth keeping, and failed attempts **only** where knowing they failed constrains future work.
- **Uncertainty & open disagreements** — what is unverified or unresolved.

Rules: Record only what remains true and useful for future work; drop transient chatter, resolved dead-ends, and step-by-step narration. Preserve specifics — exact names, paths, commands, values, and error signatures — over vague description. Mark anything unverified as an assumption; never invent facts, decisions, or outcomes you cannot support from this session, and say plainly when something important is unknown. **Never include secrets, tokens, keys, or credentials.** Be concise: prefer the shortest form a replacement could act on without re-deriving it. Output only the note (or the `No durable context.` sentinel)."#;

        assert_eq!(default_compaction_summary_prompt(), expected);
    }

    #[test]
    fn hermes_profile_launch_entries_synthesize_ready_and_unavailable() {
        use crate::backend::hermes::HermesLaunchProfileInfo;

        let infos = vec![
            HermesLaunchProfileInfo {
                name: "default".to_string(),
                summary: Some("openrouter/minimax-m3".to_string()),
                error: None,
            },
            HermesLaunchProfileInfo {
                name: "claude".to_string(),
                summary: Some("anthropic/claude-sonnet-5".to_string()),
                error: None,
            },
            HermesLaunchProfileInfo {
                name: "gpt".to_string(),
                summary: None,
                error: Some("no authenticated providers".to_string()),
            },
        ];
        let mut entries = Vec::new();
        synthesize_hermes_profile_entries(&infos, &mut entries);

        // The default profile is covered by "hermes:default", never duplicated.
        assert_eq!(entries.len(), 2);
        match &entries[0] {
            LaunchProfileEntry::Ready { profile } => {
                assert_eq!(profile.id.0, "hermes:profile:claude");
                assert_eq!(profile.label, "Hermes — claude");
                assert_eq!(profile.backend_kind, protocol::BackendKind::Hermes);
                assert_eq!(
                    profile.session_settings.0.get("profile"),
                    Some(&protocol::SessionSettingValue::String("claude".to_string()))
                );
                assert!(
                    profile
                        .description
                        .as_deref()
                        .is_some_and(|d| d.contains("anthropic/claude-sonnet-5"))
                );
            }
            other => panic!("expected ready entry, got {other:?}"),
        }
        match &entries[1] {
            LaunchProfileEntry::Unavailable {
                id, label, message, ..
            } => {
                assert_eq!(id.0, "hermes:profile:gpt");
                assert_eq!(label, "Hermes — gpt");
                assert!(message.contains("no authenticated providers"), "{message}");
            }
            other => panic!("expected unavailable entry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn passive_adapter_ingress_is_isolated_per_host_channel() {
        let (host_one_spawn_tx, _host_one_spawn_rx) = mpsc::unbounded_channel();
        let (host_two_spawn_tx, _host_two_spawn_rx) = mpsc::unbounded_channel();
        let (host_one_capacity_tx, mut host_one_capacity_rx) = mpsc::unbounded_channel();
        let (host_two_capacity_tx, mut host_two_capacity_rx) = mpsc::unbounded_channel();
        let host_one_emitter = HostSubAgentEmitter::new(
            host_one_spawn_tx,
            host_one_capacity_tx,
            AgentId("host-one-agent".to_owned()),
            Vec::new(),
        );
        let host_two_emitter = HostSubAgentEmitter::new(
            host_two_spawn_tx,
            host_two_capacity_tx,
            AgentId("host-two-agent".to_owned()),
            Vec::new(),
        );

        assert!(crate::backend::claude::forward_passive_rate_limit_event(
            &serde_json::json!({"type":"rate_limit_event","rate_limit_info":{
                "status":"allowed","rateLimitType":"five_hour","utilization":0.25
            }}),
            &host_one_emitter,
        ));
        crate::backend::codex::forward_passive_rate_limits_updated(
            &serde_json::json!({"rateLimits":{
                "limitId":"subscription","limitName":"subscription",
                "primary":{"usedPercent":50,"windowDurationMins":300,"resetsAt":1},
                "secondary":{"usedPercent":10,"windowDurationMins":10080,"resetsAt":2},
                "credits":{"hasCredits":true,"unlimited":false,"balance":"4"},
                "individualLimit":true,"planType":"pro","rateLimitReachedType":null
            }}),
            &host_two_emitter,
        );

        assert!(matches!(
            host_one_capacity_rx.recv().await,
            Some(HostCapacityUpdate::Report {
                backend_kind: BackendKind::Claude,
                ..
            })
        ));
        assert!(matches!(
            host_two_capacity_rx.recv().await,
            Some(HostCapacityUpdate::Report {
                backend_kind: BackendKind::Codex,
                ..
            })
        ));
        assert!(crate::backend::claude::forward_passive_rate_limit_event(
            &serde_json::json!({"type":"rate_limit_event","rate_limit_info":{
                "status":"allowed","rateLimitType":"five_hour","utilization":2.0
            }}),
            &host_one_emitter,
        ));
        assert!(matches!(
            host_one_capacity_rx.recv().await,
            Some(HostCapacityUpdate::Report {
                backend_kind: BackendKind::Claude,
                state: BackendCapacityState::Unavailable {
                    reason: protocol::CapacityUnavailableReason::MalformedReport,
                },
            })
        ));
        assert!(host_one_capacity_rx.try_recv().is_err());
        assert!(host_two_capacity_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn backend_native_child_reply_drop_is_a_typed_error_not_a_panic() {
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel();
        let (capacity_tx, _capacity_rx) = mpsc::unbounded_channel();
        let parent_agent_id = AgentId("cd8b1f82-a9c0-4f50-bb89-6f61fc71f2c8".to_owned());
        let emitter = Arc::new(HostSubAgentEmitter::new(
            spawn_tx,
            capacity_tx,
            parent_agent_id.clone(),
            vec!["/Users/mike/Tyggs/Tyde".to_owned()],
        ));
        let waiting = {
            let emitter = Arc::clone(&emitter);
            tokio::spawn(async move {
                emitter
                    .on_subagent_spawned(
                        "019f60f0-7a69-73f0-9ab3-7ddc24062e30".to_owned(),
                        "/root/quick_child".to_owned(),
                        "/root/quick_child".to_owned(),
                        "sub-agent".to_owned(),
                        Some(SessionId("native-quick-child-thread".to_owned())),
                    )
                    .await
            })
        };

        let request = spawn_rx.recv().await.expect("native child spawn request");
        assert_eq!(request.parent_agent_id, parent_agent_id);
        assert_eq!(request.tool_use_id, "019f60f0-7a69-73f0-9ab3-7ddc24062e30");
        assert_eq!(request.name, "/root/quick_child");
        drop(request);

        let error = match waiting.await.expect("child relay caller must not panic") {
            Ok(_) => panic!("dropped host reply must be surfaced to the adapter"),
            Err(error) => error,
        };
        assert!(error.contains("backend-native child spawn reply dropped"));
        assert!(error.contains(parent_agent_id.0.as_str()));
    }

    #[tokio::test]
    async fn session_registration_precedes_later_spawn_work_for_native_children() {
        let fixture = compact_fixture().await;
        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Early Native Child Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start parent".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });

        hook.wait_until_reached().await;
        let parent_agent_id = {
            let state = fixture.host.state.lock().await;
            let agent_ids = state.registry.agent_ids();
            assert_eq!(agent_ids.len(), 1, "only the blocked parent is registered");
            agent_ids
                .into_iter()
                .next()
                .expect("blocked parent agent id")
        };
        let emitter = {
            let state = fixture.host.state.lock().await;
            HostSubAgentEmitter::new(
                state.sub_agent_spawn_tx.clone(),
                state.capacity_tx.clone(),
                parent_agent_id.clone(),
                Vec::new(),
            )
        };
        let child_session_id = SessionId("native-quick-child-thread".to_owned());
        let child = tokio::time::timeout(
            Duration::from_millis(500),
            emitter.on_subagent_spawned(
                "019f60f0-7a69-73f0-9ab3-7ddc24062e30".to_owned(),
                "/root/quick_child".to_owned(),
                "reply exactly QUICK_DONE".to_owned(),
                "sub-agent".to_owned(),
                Some(child_session_id.clone()),
            ),
        )
        .await
        .expect("early child relay must not wait for the five-second parent-session poll")
        .expect("early child relay must be created while later parent spawn work is blocked");

        let child_handle = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_handle(&child.agent_id)
                .expect("registered native child relay")
        };
        let prompt_events = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let events = child_handle
                    .read_output(None, 100)
                    .await
                    .expect("native child output");
                let prompt_count = events
                    .iter()
                    .filter_map(|event| event.parse_payload::<ChatEvent>().ok())
                    .filter(|event| {
                        matches!(
                            event,
                            ChatEvent::MessageAdded(ChatMessage {
                                sender: MessageSender::User,
                                content,
                                ..
                            }) if content == "reply exactly QUICK_DONE"
                        )
                    })
                    .count();
                if prompt_count > 0 {
                    break (events, prompt_count);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("native child prompt must enter its relay history");
        assert_eq!(prompt_events.1, 1, "the child prompt is recorded once");
        let replayed_prompt_count = child_handle
            .read_output(None, 100)
            .await
            .expect("replayed native child output")
            .iter()
            .filter_map(|event| event.parse_payload::<ChatEvent>().ok())
            .filter(|event| {
                matches!(
                    event,
                    ChatEvent::MessageAdded(ChatMessage {
                        sender: MessageSender::User,
                        content,
                        ..
                    }) if content == "reply exactly QUICK_DONE"
                )
            })
            .count();
        assert_eq!(
            replayed_prompt_count, 1,
            "replay must not duplicate the prompt"
        );

        {
            let state = fixture.host.state.lock().await;
            assert_eq!(
                state.agent_sessions.len(),
                1,
                "only the native child relay is publicly session-bound before parent publication"
            );
            assert!(
                !state.agent_sessions.contains_key(&parent_agent_id),
                "the parent session must remain private before the original publication point"
            );
            assert!(
                state.pending_agent_sessions.contains_key(&parent_agent_id),
                "the scheduled parent session registration must be available to native child resolution"
            );
            assert_eq!(
                state.agent_sessions.get(&child.agent_id),
                Some(&child_session_id),
                "the real host relay must retain the authoritative native child session"
            );
            let child_start = state
                .registry
                .agent_handle(&child.agent_id)
                .expect("registered native child relay")
                .snapshot();
            assert_eq!(child_start.parent_agent_id, Some(parent_agent_id.clone()));
            assert_eq!(child_start.origin, AgentOrigin::BackendNative);
        }

        hook.resume();
        let spawned_parent = spawn
            .await
            .expect("parent spawn task")
            .expect("parent spawn succeeds after later work resumes");
        assert_eq!(spawned_parent, parent_agent_id);
        let public_parent_session_id = tokio::time::timeout(
            Duration::from_millis(500),
            fixture
                .host
                .wait_for_agent_session_id_result(&parent_agent_id),
        )
        .await
        .expect("parent session publication must complete after fanout resumes")
        .expect("published parent session id");
        {
            let state = fixture.host.state.lock().await;
            assert_eq!(state.agent_sessions.len(), 2);
            assert_eq!(
                state.agent_sessions.get(&parent_agent_id),
                Some(&public_parent_session_id)
            );
            assert!(!state.pending_agent_sessions.contains_key(&parent_agent_id));
        }
        assert!(fixture.host.close_agent(&parent_agent_id).await);
        drop(child);
    }

    #[tokio::test]
    async fn early_visible_native_child_closes_without_exposing_its_parent() {
        let fixture = compact_fixture().await;
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        fixture
            .host
            .register_host_stream(
                Stream::new(
                    StreamPath(format!("/host/early-child-first-{}", Uuid::new_v4())),
                    first_tx,
                ),
                AgentReplayMode::Lazy,
            )
            .await;
        let (second_tx, mut second_rx) = mpsc::unbounded_channel();
        fixture
            .host
            .register_host_stream(
                Stream::new(
                    StreamPath(format!("/host/early-child-second-{}", Uuid::new_v4())),
                    second_tx,
                ),
                AgentReplayMode::Lazy,
            )
            .await;
        while first_rx.try_recv().is_ok() {}
        while second_rx.try_recv().is_ok() {}

        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Hidden Parent With Early Native Child".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start hidden parent".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        hook.wait_until_reached().await;
        let parent_agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("blocked parent agent")
        };
        let emitter = {
            let state = fixture.host.state.lock().await;
            HostSubAgentEmitter::new(
                state.sub_agent_spawn_tx.clone(),
                state.capacity_tx.clone(),
                parent_agent_id.clone(),
                Vec::new(),
            )
        };
        let child = tokio::time::timeout(
            Duration::from_millis(500),
            emitter.on_subagent_spawned(
                "019f60f0-7a69-73f0-9ab3-7ddc24062e30".to_owned(),
                "/root/quick_child".to_owned(),
                "/root/quick_child".to_owned(),
                "sub-agent".to_owned(),
                Some(SessionId("native-quick-child-thread".to_owned())),
            ),
        )
        .await
        .expect("early native child relay must not wait for parent publication")
        .expect("early native child relay");
        let child_agent_id = child.agent_id.clone();

        {
            let state = fixture.host.state.lock().await;
            assert!(
                state
                    .agent_visibility
                    .visible_host_streams(&parent_agent_id)
                    .is_empty(),
                "the blocked parent must have zero public NewAgent visibility"
            );
            assert_eq!(
                state
                    .agent_visibility
                    .visible_host_streams(&child_agent_id)
                    .len(),
                2,
                "the independently published native child must retain its own subscribers"
            );
        }

        spawn.abort();
        assert!(
            spawn
                .await
                .expect_err("cancelled parent spawn")
                .is_cancelled()
        );
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&parent_agent_id).is_none()
                    && state.registry.agent_handle(&child_agent_id).is_none()
                    && !state.agent_sessions.contains_key(&parent_agent_id)
                    && !state.agent_sessions.contains_key(&child_agent_id)
                    && !state.pending_agent_sessions.contains_key(&parent_agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&parent_agent_id)
                        .is_empty()
                    && state
                        .agent_visibility
                        .visible_host_streams(&child_agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("parent cancellation must clean parent and independently visible child state");

        tokio::time::sleep(Duration::from_millis(50)).await;
        for receiver in [&mut first_rx, &mut second_rx] {
            let mut child_lifecycle = Vec::new();
            let mut parent_lifecycle = Vec::new();
            while let Ok(envelope) = receiver.try_recv() {
                match envelope.kind {
                    FrameKind::NewAgent => {
                        let payload: NewAgentPayload =
                            envelope.parse_payload().expect("NewAgent payload");
                        if payload.agent_id == child_agent_id {
                            child_lifecycle.push("new");
                        } else if payload.agent_id == parent_agent_id {
                            parent_lifecycle.push("new");
                        }
                    }
                    FrameKind::AgentClosed => {
                        let payload: AgentClosedPayload =
                            envelope.parse_payload().expect("AgentClosed payload");
                        if payload.agent_id == child_agent_id {
                            child_lifecycle.push("closed");
                        } else if payload.agent_id == parent_agent_id {
                            parent_lifecycle.push("closed");
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(child_lifecycle, vec!["new", "closed"]);
            assert!(
                parent_lifecycle.is_empty(),
                "a zero-visibility parent must not produce orphan, reversed, or duplicate lifecycle events: {parent_lifecycle:?}"
            );
        }
        drop(child);
    }

    #[tokio::test]
    async fn bootstrap_records_visible_native_child_but_omits_pending_parent() {
        let fixture = compact_fixture().await;
        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Bootstrap Hidden Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start hidden parent before bootstrap".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        hook.wait_until_reached().await;
        let parent_agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("blocked parent agent")
        };
        let emitter = {
            let state = fixture.host.state.lock().await;
            HostSubAgentEmitter::new(
                state.sub_agent_spawn_tx.clone(),
                state.capacity_tx.clone(),
                parent_agent_id.clone(),
                Vec::new(),
            )
        };
        let child = tokio::time::timeout(
            Duration::from_millis(500),
            emitter.on_subagent_spawned(
                "019f60f0-7a69-73f0-9ab3-7ddc24062e30".to_owned(),
                "/root/quick_child".to_owned(),
                "/root/quick_child".to_owned(),
                "sub-agent".to_owned(),
                Some(SessionId("native-quick-child-thread".to_owned())),
            ),
        )
        .await
        .expect("early child spawn before bootstrap")
        .expect("early child relay");
        let child_agent_id = child.agent_id.clone();

        let (tx, mut rx) = mpsc::unbounded_channel();
        fixture
            .host
            .register_host_stream(
                Stream::new(
                    StreamPath(format!("/host/bootstrap-child-{}", Uuid::new_v4())),
                    tx,
                ),
                AgentReplayMode::Lazy,
            )
            .await;
        let bootstrap = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let envelope = rx.recv().await.expect("host stream remains open");
                if envelope.kind == FrameKind::HostBootstrap {
                    return envelope
                        .parse_payload::<HostBootstrapPayload>()
                        .expect("HostBootstrap payload");
                }
            }
        })
        .await
        .expect("bootstrap delivery");
        assert!(
            bootstrap
                .agents
                .iter()
                .any(|agent| agent.agent_id == child_agent_id)
        );
        assert!(
            !bootstrap
                .agents
                .iter()
                .any(|agent| agent.agent_id == parent_agent_id)
        );
        {
            let state = fixture.host.state.lock().await;
            assert_eq!(
                state
                    .agent_visibility
                    .visible_host_streams(&child_agent_id)
                    .len(),
                1,
                "successful bootstrap must become the child visibility recipient"
            );
            assert!(
                state
                    .agent_visibility
                    .visible_host_streams(&parent_agent_id)
                    .is_empty()
            );
        }

        spawn.abort();
        assert!(
            spawn
                .await
                .expect_err("cancelled parent spawn")
                .is_cancelled()
        );
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&parent_agent_id).is_none()
                    && state.registry.agent_handle(&child_agent_id).is_none()
                    && !state.agent_sessions.contains_key(&parent_agent_id)
                    && !state.agent_sessions.contains_key(&child_agent_id)
                    && !state.pending_agent_sessions.contains_key(&parent_agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&parent_agent_id)
                        .is_empty()
                    && state
                        .agent_visibility
                        .visible_host_streams(&child_agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled bootstrap parent and child cleanup");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut child_closed = 0usize;
        let mut parent_closed = 0usize;
        let mut child_new = 0usize;
        let mut parent_new = 0usize;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("NewAgent payload");
                    if payload.agent_id == child_agent_id {
                        child_new += 1;
                    } else if payload.agent_id == parent_agent_id {
                        parent_new += 1;
                    }
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("AgentClosed payload");
                    if payload.agent_id == child_agent_id {
                        child_closed += 1;
                    } else if payload.agent_id == parent_agent_id {
                        parent_closed += 1;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(
            child_new, 0,
            "bootstrap must not duplicate its child as NewAgent"
        );
        assert_eq!(parent_new, 0, "hidden parent must not replay as NewAgent");
        assert_eq!(
            child_closed, 1,
            "bootstrapped child must close exactly once"
        );
        assert_eq!(parent_closed, 0, "hidden parent must not emit AgentClosed");
        drop(child);
    }

    #[tokio::test]
    async fn cancelled_spawn_cleans_unpublished_session_registration() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_stream = Stream::new(
            StreamPath(format!("/host/cancelled-unpublished-{}", Uuid::new_v4())),
            tx,
        );
        fixture
            .host
            .register_host_stream(host_stream, AgentReplayMode::Lazy)
            .await;
        while rx.try_recv().is_ok() {}
        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Cancelled Pending Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start then cancel".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });

        hook.wait_until_reached().await;
        let agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("blocked parent agent")
        };
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let pending = fixture
                    .host
                    .state
                    .lock()
                    .await
                    .pending_agent_sessions
                    .contains_key(&agent_id);
                if pending {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled parent reaches pending private session registration");
        spawn.abort();
        assert!(
            spawn
                .await
                .expect_err("cancelled spawn task")
                .is_cancelled()
        );

        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&agent_id).is_none()
                    && !state.agent_sessions.contains_key(&agent_id)
                    && !state.pending_agent_sessions.contains_key(&agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled spawn must not leave an agent or session binding behind");

        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("NewAgent payload");
                    assert_ne!(
                        payload.agent_id, agent_id,
                        "cancelled unpublished agent must never become visible"
                    );
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("AgentClosed payload");
                    assert_ne!(
                        payload.agent_id, agent_id,
                        "cancelled unpublished agent must not emit orphan AgentClosed"
                    );
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn startup_failure_cleans_unpublished_session_registration() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_stream = Stream::new(
            StreamPath(format!("/host/failed-unpublished-{}", Uuid::new_v4())),
            tx,
        );
        fixture
            .host
            .register_host_stream(host_stream, AgentReplayMode::Lazy)
            .await;
        while rx.try_recv().is_ok() {}
        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let cleanup_hook = install_spawn_cancelled_before_cleanup_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Failed Pending Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "__mock_fail_spawn__".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        hook.wait_until_reached().await;
        cleanup_hook.wait_until_reached().await;
        let agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("blocked failed parent agent")
        };
        cleanup_hook.resume();
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&agent_id).is_none()
                    && !state.agent_sessions.contains_key(&agent_id)
                    && !state.pending_agent_sessions.contains_key(&agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("startup failure must not leave an unpublished session binding behind");
        hook.resume();
        let spawned_agent_id = spawn
            .await
            .expect("startup-failure spawn task")
            .expect("spawn request is accepted before asynchronous startup failure");
        assert_eq!(spawned_agent_id, agent_id);

        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("NewAgent payload");
                    assert_ne!(
                        payload.agent_id, agent_id,
                        "startup failure must not resurrect an unpublished agent"
                    );
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("AgentClosed payload");
                    assert_ne!(
                        payload.agent_id, agent_id,
                        "startup failure must not emit AgentClosed before NewAgent"
                    );
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn simultaneous_startup_failure_and_fanout_publish_one_terminal_agent() {
        let _race_guard = STARTUP_FAILURE_FANOUT_RACE_TEST_LOCK.lock().await;
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/failed-fanout-race-{}", Uuid::new_v4()));
        fixture
            .host
            .register_host_stream(Stream::new(host_path.clone(), tx), AgentReplayMode::Eager)
            .await;
        while rx.try_recv().is_ok() {}

        let hook = install_startup_failure_fanout_race_test_hook(
            &fixture.host,
            StartupFailureFanoutRaceWinner::Fanout,
        );
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Failed Fanout Race Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "__mock_fail_spawn__".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });

        hook.wait_until_ready().await;
        let agent_id = spawn
            .await
            .expect("startup-failure fanout race task")
            .expect("spawn request remains accepted when fanout wins publication");

        let envelopes = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let (new_agent_index, instance_stream) = envelopes
            .iter()
            .enumerate()
            .find_map(|(index, envelope)| {
                if envelope.kind != FrameKind::NewAgent {
                    return None;
                }
                let payload: NewAgentPayload = envelope.parse_payload().expect("NewAgent payload");
                (payload.agent_id == agent_id).then_some((index, payload.instance_stream))
            })
            .expect("fanout-winning startup failure must publish NewAgent");
        let (bootstrap_index, bootstrap) = envelopes
            .iter()
            .enumerate()
            .find_map(|(index, envelope)| {
                if envelope.stream != instance_stream || envelope.kind != FrameKind::AgentBootstrap
                {
                    return None;
                }
                Some((
                    index,
                    envelope
                        .parse_payload::<protocol::AgentBootstrapPayload>()
                        .expect("terminal AgentBootstrap payload"),
                ))
            })
            .expect("published failed agent must receive terminal bootstrap");
        assert!(
            bootstrap_index > new_agent_index,
            "terminal bootstrap must follow NewAgent publication"
        );
        assert!(bootstrap.events.iter().any(|event| matches!(
            event,
            protocol::AgentBootstrapEvent::AgentStart(start) if start.agent_id == agent_id
        )));
        assert!(bootstrap.events.iter().any(|event| matches!(
            event,
            protocol::AgentBootstrapEvent::AgentError(error)
                if error.fatal && error.message.contains("mock backend forced spawn failure")
        )));
        assert!(envelopes.iter().all(|envelope| {
            envelope.kind != FrameKind::AgentClosed
                || envelope
                    .parse_payload::<AgentClosedPayload>()
                    .expect("AgentClosed payload")
                    .agent_id
                    != agent_id
        }));

        let status = fixture
            .host
            .agent_status_snapshot(&agent_id)
            .await
            .expect("published terminal failed agent remains registered");
        assert!(status.terminated);
        assert_eq!(status.status(), AgentControlStatus::Failed);
        let state = fixture.host.state.lock().await;
        assert!(state.registry.agent_handle(&agent_id).is_some());
        assert!(!state.agent_sessions.contains_key(&agent_id));
        assert!(!state.pending_agent_sessions.contains_key(&agent_id));
        assert_eq!(
            state.agent_visibility.visible_host_streams(&agent_id),
            HashSet::from([host_path])
        );
    }

    #[tokio::test]
    async fn simultaneous_startup_failure_claim_prevents_unpublished_fanout() {
        let _race_guard = STARTUP_FAILURE_FANOUT_RACE_TEST_LOCK.lock().await;
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        fixture
            .host
            .register_host_stream(
                Stream::new(
                    StreamPath(format!("/host/failed-unpublished-race-{}", Uuid::new_v4())),
                    tx,
                ),
                AgentReplayMode::Eager,
            )
            .await;
        while rx.try_recv().is_ok() {}

        let race_hook = install_startup_failure_fanout_race_test_hook(
            &fixture.host,
            StartupFailureFanoutRaceWinner::Failure,
        );
        let cleanup_hook = install_spawn_cancelled_before_cleanup_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Failed Unpublished Race Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "__mock_fail_spawn__".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });

        race_hook.wait_until_ready().await;
        cleanup_hook.wait_until_reached().await;
        let agent_id = spawn
            .await
            .expect("startup-failure cancellation race task")
            .expect("spawn request remains accepted when startup failure cancels publication");

        cleanup_hook.resume();
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&agent_id).is_none()
                    && !state.agent_sessions.contains_key(&agent_id)
                    && !state.pending_agent_sessions.contains_key(&agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("failure-first atomic claim must clean unpublished state");
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("NewAgent payload");
                    assert_ne!(payload.agent_id, agent_id);
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("AgentClosed payload");
                    assert_ne!(payload.agent_id, agent_id);
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn synchronous_parent_fanout_closes_every_advertised_subscriber() {
        let fixture = compact_fixture().await;
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        let first_stream = Stream::new(
            StreamPath(format!("/host/partial-first-{}", Uuid::new_v4())),
            first_tx,
        );
        fixture
            .host
            .register_host_stream(first_stream, AgentReplayMode::Lazy)
            .await;
        let (second_tx, mut second_rx) = mpsc::unbounded_channel();
        let second_stream = Stream::new(
            StreamPath(format!("/host/partial-second-{}", Uuid::new_v4())),
            second_tx,
        );
        fixture
            .host
            .register_host_stream(second_stream, AgentReplayMode::Lazy)
            .await;
        while first_rx.try_recv().is_ok() {}
        while second_rx.try_recv().is_ok() {}

        let fanout_hook = install_spawn_new_agent_fanout_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Partial Fanout Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start then cancel during fanout".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });

        fanout_hook.wait_until_reached().await;
        let agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("partially visible parent agent")
        };
        spawn.abort();
        assert!(
            spawn
                .await
                .expect_err("cancelled fanout task")
                .is_cancelled()
        );
        drop(fanout_hook);

        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&agent_id).is_none()
                    && !state.agent_sessions.contains_key(&agent_id)
                    && !state.pending_agent_sessions.contains_key(&agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("partial fanout cancellation must clean all agent bindings and visibility");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut first_lifecycle = Vec::new();
        while let Ok(envelope) = first_rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("first NewAgent payload");
                    if payload.agent_id == agent_id {
                        first_lifecycle.push("new");
                    }
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("first AgentClosed payload");
                    if payload.agent_id == agent_id {
                        first_lifecycle.push("closed");
                    }
                }
                _ => {}
            }
        }
        let mut second_lifecycle = Vec::new();
        while let Ok(envelope) = second_rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("second NewAgent payload");
                    if payload.agent_id == agent_id {
                        second_lifecycle.push("new");
                    }
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload = envelope
                        .parse_payload()
                        .expect("second AgentClosed payload");
                    if payload.agent_id == agent_id {
                        second_lifecycle.push("closed");
                    }
                }
                _ => {}
            }
        }

        assert_eq!(
            first_lifecycle,
            vec!["new", "closed"],
            "sorted host fanout must preserve NewAgent before AgentClosed for its visible subscriber"
        );
        assert_eq!(
            second_lifecycle,
            vec!["new", "closed"],
            "two-phase fanout advertises every eligible subscriber before cancellation"
        );
    }

    #[tokio::test]
    async fn bootstrap_includes_visible_unpublished_normal_spawn_once() {
        let fixture = compact_fixture().await;
        let hook = install_spawn_visible_before_publication_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Visible Before Session Publication".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "complete fanout before session publication".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        hook.wait_until_reached().await;
        let agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("visible unpublished agent")
        };
        {
            let state = fixture.host.state.lock().await;
            assert!(
                !state.agent_sessions.contains_key(&agent_id),
                "test hook must stop before public session promotion"
            );
            assert!(
                state.agent_visibility.bootstrap_eligible(&agent_id, false),
                "completed NewAgent fanout must make the agent bootstrap-eligible"
            );
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        fixture
            .host
            .register_host_stream(
                Stream::new(
                    StreamPath(format!("/host/visible-before-publish-{}", Uuid::new_v4())),
                    tx,
                ),
                AgentReplayMode::Lazy,
            )
            .await;
        let bootstrap = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let envelope = rx.recv().await.expect("host stream remains open");
                if envelope.kind == FrameKind::HostBootstrap {
                    return envelope
                        .parse_payload::<HostBootstrapPayload>()
                        .expect("HostBootstrap payload");
                }
            }
        })
        .await
        .expect("bootstrap delivery");
        assert_eq!(
            bootstrap
                .agents
                .iter()
                .filter(|agent| agent.agent_id == agent_id)
                .count(),
            1,
            "visible normal spawn must appear exactly once in bootstrap"
        );
        {
            let state = fixture.host.state.lock().await;
            assert_eq!(
                state.agent_visibility.visible_host_streams(&agent_id).len(),
                1,
                "successful bootstrap must record its host visibility recipient"
            );
        }

        hook.resume();
        let spawned_agent_id = spawn
            .await
            .expect("visible normal spawn task")
            .expect("visible normal spawn result");
        assert_eq!(spawned_agent_id, agent_id);
        tokio::time::timeout(
            Duration::from_millis(500),
            fixture.host.wait_for_agent_session_id_result(&agent_id),
        )
        .await
        .expect("session publication after hook resume")
        .expect("published session");
        assert!(fixture.host.close_agent(&agent_id).await);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut new_agent_count = 0usize;
        let mut closed_count = 0usize;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("NewAgent payload");
                    if payload.agent_id == agent_id {
                        new_agent_count += 1;
                    }
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("AgentClosed payload");
                    if payload.agent_id == agent_id {
                        closed_count += 1;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(
            new_agent_count, 0,
            "bootstrap must deduplicate pending NewAgent replay"
        );
        assert_eq!(
            closed_count, 1,
            "visible normal spawn must close exactly once"
        );
    }

    #[tokio::test]
    async fn cancelled_visibility_excludes_even_a_publicly_bound_agent_from_bootstrap() {
        let fixture = compact_fixture().await;
        let visible_hook = install_spawn_visible_before_publication_test_hook(&fixture.host);
        let cancelled_hook = install_spawn_cancelled_before_cleanup_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Cancelled Before Bootstrap".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "become visible then cancel".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        visible_hook.wait_until_reached().await;
        let agent_id = {
            let mut state = fixture.host.state.lock().await;
            let agent_id = state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("visible agent before cancellation");
            state.agent_sessions.insert(
                agent_id.clone(),
                SessionId("test-public-binding-before-cancellation".to_owned()),
            );
            agent_id
        };

        spawn.abort();
        assert!(
            spawn
                .await
                .expect_err("cancelled visible spawn")
                .is_cancelled()
        );
        cancelled_hook.wait_until_reached().await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        fixture
            .host
            .register_host_stream(
                Stream::new(
                    StreamPath(format!("/host/cancelled-bootstrap-{}", Uuid::new_v4())),
                    tx,
                ),
                AgentReplayMode::Lazy,
            )
            .await;
        let bootstrap = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let envelope = rx.recv().await.expect("host stream remains open");
                if envelope.kind == FrameKind::HostBootstrap {
                    return envelope
                        .parse_payload::<HostBootstrapPayload>()
                        .expect("HostBootstrap payload");
                }
            }
        })
        .await
        .expect("bootstrap delivery during cancelled cleanup");
        assert!(
            !bootstrap
                .agents
                .iter()
                .any(|agent| agent.agent_id == agent_id),
            "cancelled visibility must override a stale public binding"
        );

        cancelled_hook.resume();
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = fixture.host.state.lock().await;
                let cleaned = state.registry.agent_handle(&agent_id).is_none()
                    && !state.agent_sessions.contains_key(&agent_id)
                    && !state.pending_agent_sessions.contains_key(&agent_id)
                    && state
                        .agent_visibility
                        .visible_host_streams(&agent_id)
                        .is_empty();
                drop(state);
                if cleaned {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled visibility cleanup");
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::NewAgent => {
                    let payload: NewAgentPayload =
                        envelope.parse_payload().expect("NewAgent payload");
                    assert_ne!(payload.agent_id, agent_id);
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("AgentClosed payload");
                    assert_ne!(payload.agent_id, agent_id);
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn session_publication_follows_new_agent_fanout() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_stream = Stream::new(
            StreamPath(format!(
                "/host/session-publication-order-{}",
                Uuid::new_v4()
            )),
            tx,
        );
        let attachments = fixture
            .host
            .register_host_stream(host_stream, AgentReplayMode::Lazy)
            .await;
        assert!(
            attachments.is_empty(),
            "empty host has no replay attachments"
        );
        while rx.try_recv().is_ok() {}

        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Publication Ordering Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start ordered parent".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        hook.wait_until_reached().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), async {
                loop {
                    let envelope = rx.recv().await.expect("host stream remains open");
                    if matches!(
                        envelope.kind,
                        FrameKind::NewAgent
                            | FrameKind::AgentsViewPreferencesNotify
                            | FrameKind::SessionList
                    ) {
                        return envelope;
                    }
                }
            })
            .await
            .is_err(),
            "pending session registration must not publish host or preference events before fanout"
        );

        hook.resume();
        let parent_agent_id = spawn
            .await
            .expect("ordered parent spawn task")
            .expect("ordered parent spawn succeeds");
        let mut new_agent_index = None;
        let mut preference_index = None;
        let mut session_list_index = None;
        let mut index = 0usize;
        tokio::time::timeout(Duration::from_millis(500), async {
            while new_agent_index.is_none()
                || preference_index.is_none()
                || session_list_index.is_none()
            {
                let envelope = rx.recv().await.expect("host stream remains open");
                match envelope.kind {
                    FrameKind::NewAgent => {
                        let payload: NewAgentPayload =
                            envelope.parse_payload().expect("NewAgent payload");
                        if payload.agent_id == parent_agent_id {
                            new_agent_index = Some(index);
                        }
                    }
                    FrameKind::AgentsViewPreferencesNotify => preference_index = Some(index),
                    FrameKind::SessionList => session_list_index = Some(index),
                    _ => {}
                }
                index += 1;
            }
        })
        .await
        .expect("parent fanout and published session events");
        let new_agent_index = new_agent_index.expect("parent NewAgent index");
        let preference_index = preference_index.expect("preferences publication index");
        let session_list_index = session_list_index.expect("session-list publication index");
        assert!(new_agent_index < preference_index);
        assert!(preference_index < session_list_index);
        assert!(fixture.host.close_agent(&parent_agent_id).await);
    }

    #[tokio::test]
    async fn pending_agent_annotation_promotes_only_at_session_publication() {
        let fixture = compact_fixture().await;
        let hook = install_spawn_session_registration_test_hook(&fixture.host);
        let spawning_host = fixture.host.clone();
        let spawn = tokio::spawn(async move {
            spawning_host
                .spawn_agent(SpawnAgentPayload {
                    name: Some("Pending Annotation Parent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "start annotation parent".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                })
                .await
        });
        hook.wait_until_reached().await;
        let agent_id = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_ids()
                .into_iter()
                .next()
                .expect("blocked parent agent")
        };
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let pending = fixture
                    .host
                    .state
                    .lock()
                    .await
                    .pending_agent_sessions
                    .contains_key(&agent_id);
                if pending {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("parent startup must create its private pending session binding");

        fixture
            .host
            .set_agent_tags(SetAgentTagsPayload {
                update: AgentTagsUpdate::CreateTag {
                    name: "Pending session tag".to_owned(),
                    color: None,
                },
            })
            .await
            .expect("create pending-session tag");
        let tag_id = {
            let store = fixture
                .host
                .state
                .lock()
                .await
                .agents_view_preferences_store
                .clone()
                .expect("primary preferences store");
            store
                .lock()
                .await
                .snapshot()
                .tags
                .manual
                .into_iter()
                .find(|tag| tag.name == "Pending session tag")
                .expect("created tag")
                .id
        };
        let transient_target = AgentAnnotationTarget::TransientAgent {
            host_id: HostFilterId(LOCAL_HOST_ID.to_owned()),
            agent_id: agent_id.clone(),
        };
        fixture
            .host
            .set_agent_tags(SetAgentTagsPayload {
                update: AgentTagsUpdate::AssignTag {
                    target: transient_target.clone(),
                    tag_id: tag_id.clone(),
                },
            })
            .await
            .expect("assign tag while session is pending");
        {
            let state = fixture.host.state.lock().await;
            assert!(!state.agent_sessions.contains_key(&agent_id));
            assert!(state.pending_agent_sessions.contains_key(&agent_id));
            let snapshot = state
                .agents_view_preferences_store
                .as_ref()
                .expect("primary preferences store")
                .lock()
                .await
                .snapshot();
            assert!(snapshot.tags.manual_assignments.iter().any(|assignment| {
                assignment.target == transient_target && assignment.tag_ids.contains(&tag_id)
            }));
        }

        hook.resume();
        let spawned_agent_id = spawn
            .await
            .expect("annotation parent spawn task")
            .expect("annotation parent spawn succeeds");
        assert_eq!(spawned_agent_id, agent_id);
        let session_id = tokio::time::timeout(
            Duration::from_millis(500),
            fixture.host.wait_for_agent_session_id_result(&agent_id),
        )
        .await
        .expect("session publication completes")
        .expect("published session");
        let session_target = AgentAnnotationTarget::Session {
            host_id: HostFilterId(LOCAL_HOST_ID.to_owned()),
            session_id,
        };
        {
            let state = fixture.host.state.lock().await;
            let snapshot = state
                .agents_view_preferences_store
                .as_ref()
                .expect("primary preferences store")
                .lock()
                .await
                .snapshot();
            assert!(!snapshot.tags.manual_assignments.iter().any(|assignment| {
                assignment.target == transient_target && assignment.tag_ids.contains(&tag_id)
            }));
            assert!(snapshot.tags.manual_assignments.iter().any(|assignment| {
                assignment.target == session_target && assignment.tag_ids.contains(&tag_id)
            }));
        }
        assert!(fixture.host.close_agent(&agent_id).await);
    }

    #[test]
    fn startup_mcp_servers_attach_config_only_to_help_agent() {
        let settings = protocol::HostSettings {
            enabled_backends: vec![BackendKind::Claude],
            default_backend: Some(BackendKind::Claude),
            enable_mobile_connections: false,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: false,
            complexity_tiers_enabled: false,
            backend_tier_configs: HashMap::new(),
            background_agent_features: Default::default(),
            supervisor: Default::default(),
            code_intel: Default::default(),
            backend_config: HashMap::new(),
            launch_profiles: Vec::new(),
        };
        let debug_mcp = DebugMcpHandle { url: String::new() };
        let agent_control = AgentControlMcpHandle::disabled();
        let config_mcp = ConfigMcpHandle {
            url: "http://127.0.0.1:9/mcp".to_owned(),
        };

        let help_id = CustomAgentId(crate::store::custom_agents::HELP_CUSTOM_AGENT_ID.to_owned());
        let servers = startup_mcp_servers_for_settings(
            &settings,
            &[],
            &debug_mcp,
            &agent_control,
            &config_mcp,
            Some(&help_id),
        );
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["tyde-config"],
            "help agent must get the config tools"
        );

        let other_id = CustomAgentId("ca-user".to_owned());
        for custom_agent_id in [None, Some(&other_id)] {
            let servers = startup_mcp_servers_for_settings(
                &settings,
                &[],
                &debug_mcp,
                &agent_control,
                &config_mcp,
                custom_agent_id,
            );
            assert!(
                servers.iter().all(|s| s.name != "tyde-config"),
                "non-help spawns must not get config tools: {custom_agent_id:?}"
            );
        }
    }

    #[test]
    fn antigravity_session_summary_resumability_requires_native_db() {
        let missing_id = SessionId("55a3c5e1-a2e1-44c1-9246-6e3de751803d".to_owned());
        let existing_id = SessionId("66666666-6666-4666-8666-666666666666".to_owned());
        let compacted_id = SessionId("77777777-7777-4777-8777-777777777777".to_owned());
        let backend_native_id = SessionId("88888888-8888-4888-8888-888888888888".to_owned());
        let replacement_id = SessionId("99999999-9999-4999-8999-999999999999".to_owned());
        let parent_id = SessionId("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned());
        let synthetic_id = SessionId("antigravity-legacy".to_owned());
        let claude_id = SessionId("claude-session".to_owned());
        let mut compacted_summary =
            test_session_summary(BackendKind::Antigravity, compacted_id.clone(), false);
        compacted_summary.compacted_to_session_id = Some(replacement_id);
        let mut backend_native_summary =
            test_session_summary(BackendKind::Antigravity, backend_native_id.clone(), false);
        backend_native_summary.parent_id = Some(parent_id);
        let mut summaries = vec![
            test_session_summary(BackendKind::Antigravity, missing_id.clone(), true),
            test_session_summary(BackendKind::Antigravity, existing_id.clone(), false),
            compacted_summary,
            backend_native_summary,
            test_session_summary(BackendKind::Antigravity, synthetic_id, true),
            test_session_summary(BackendKind::Claude, claude_id.clone(), true),
        ];

        normalize_antigravity_session_resumability_with(&mut summaries, |session_id| {
            session_id == &existing_id
                || session_id == &compacted_id
                || session_id == &backend_native_id
        });

        assert!(!summaries[0].resumable);
        assert!(summaries[1].resumable);
        assert!(!summaries[2].resumable);
        assert!(!summaries[3].resumable);
        assert!(!summaries[4].resumable);
        assert!(summaries[5].resumable);
    }

    fn test_session_summary(
        backend_kind: BackendKind,
        id: SessionId,
        resumable: bool,
    ) -> SessionSummary {
        SessionSummary {
            id,
            backend_kind,
            launch_profile_id: None,
            workspace_roots: Vec::new(),
            project_id: None,
            alias: None,
            user_alias: None,
            parent_id: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 0,
            token_count: None,
            resumable,
            compacted_from_session_id: None,
            compacted_to_session_id: None,
            compacted_at_ms: None,
            compaction_summary_preview: None,
        }
    }

    #[test]
    fn spawn_host_with_mock_backend_does_not_require_existing_tokio_runtime() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");

        let result = spawn_host_with_mock_backend(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
        );
        if let Err(err) = result {
            panic!("host spawn should succeed without an existing Tokio runtime: {err}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn activity_summary_state(
        host: &HostHandle,
        agent_id: &AgentId,
    ) -> AgentActivitySummaryState {
        let state = host.state.lock().await;
        current_agent_activity_summary_state(&state, agent_id)
    }

    #[tokio::test]
    async fn activity_summary_timeout_error_clears_in_flight_and_emits_error() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "timeout summary target").await;
        let mut entries = HashMap::new();
        entries.insert(
            agent_id.clone(),
            ActivitySummarySchedulerEntry {
                in_flight: true,
                ..Default::default()
            },
        );

        finish_activity_summary_call(
            &fixture.host,
            &mut entries,
            ActivitySummaryTaskResult {
                agent_id: agent_id.clone(),
                epoch: 7,
                transient_agent_id: AgentId(Uuid::new_v4().to_string()),
                source_through_seq: Some(12),
                result: Err("activity summary generation timed out after 30 seconds".to_owned()),
            },
            ActivitySummarySettingsSignal {
                enabled: true,
                epoch: 7,
            },
        )
        .await;

        let entry = entries.get(&agent_id).expect("scheduler entry");
        assert!(
            !entry.in_flight,
            "timeout result must clear the in-flight marker"
        );
        let state = activity_summary_state(&fixture.host, &agent_id).await;
        let AgentActivitySummaryState::Error { message, .. } = state else {
            panic!("expected activity summary error after timeout, got {state:?}");
        };
        assert_eq!(
            message,
            "activity summary generation timed out after 30 seconds"
        );
    }

    #[tokio::test]
    async fn activity_summary_generation_timeout_returns_error() {
        let error = await_activity_summary_generation(
            std::future::pending::<Result<AgentActivitySummary, String>>(),
            Duration::from_millis(10),
        )
        .await
        .expect_err("pending summary generation should time out");

        assert_eq!(error, "activity summary generation timed out after 10 ms");
    }

    #[tokio::test]
    async fn agent_name_generation_timeout_returns_error() {
        let error = await_agent_name_generation(
            std::future::pending::<Result<String, String>>(),
            Duration::from_millis(10),
        )
        .await
        .expect_err("pending agent name generation should time out");

        assert_eq!(error, "agent name generation timed out after 10 ms");
    }

    #[tokio::test]
    async fn queued_activity_summary_waits_for_permit_before_pending_state() {
        let fixture = compact_fixture().await;
        let (slow_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "__mock_slow_activity_summary__ keep working")
                .await;
        let (queued_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "normal queued summary").await;
        let settings = ActivitySummarySettingsSignal {
            enabled: true,
            epoch: 11,
        };
        let semaphore = Arc::new(Semaphore::new(1));
        let (task_event_tx, mut task_event_rx) = mpsc::unbounded_channel();
        let mut entries = HashMap::new();
        entries.insert(
            slow_agent_id.clone(),
            ActivitySummarySchedulerEntry {
                pending_due: Some(Instant::now() - Duration::from_millis(1)),
                ..Default::default()
            },
        );

        start_due_activity_summary_calls(
            &fixture.host,
            &mut entries,
            settings,
            Arc::clone(&semaphore),
            task_event_tx.clone(),
        )
        .await;
        let started = tokio::time::timeout(Duration::from_secs(1), task_event_rx.recv())
            .await
            .expect("slow summary should acquire the permit")
            .expect("slow summary start event");
        let ActivitySummaryTaskEvent::Started(started) = started else {
            panic!("expected slow summary start event");
        };
        assert_eq!(started.agent_id, slow_agent_id);
        begin_activity_summary_call(&fixture.host, &mut entries, started, settings).await;
        assert!(matches!(
            activity_summary_state(&fixture.host, &slow_agent_id).await,
            AgentActivitySummaryState::Pending { .. }
        ));

        entries.insert(
            queued_agent_id.clone(),
            ActivitySummarySchedulerEntry {
                pending_due: Some(Instant::now() - Duration::from_millis(1)),
                ..Default::default()
            },
        );
        start_due_activity_summary_calls(
            &fixture.host,
            &mut entries,
            settings,
            Arc::clone(&semaphore),
            task_event_tx,
        )
        .await;

        match tokio::time::timeout(Duration::from_millis(200), task_event_rx.recv()).await {
            Err(_) => {}
            Ok(Some(event)) => {
                panic!("queued summary emitted an event before the permit was free: {event:?}");
            }
            Ok(None) => panic!("activity summary task event channel closed"),
        }
        assert!(
            !matches!(
                activity_summary_state(&fixture.host, &queued_agent_id).await,
                AgentActivitySummaryState::Pending { .. }
            ),
            "queued summaries must not render as Pending before generation starts"
        );
    }

    #[tokio::test]
    async fn delete_project_removes_code_intel_router() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_root = dir.path().join("project-root");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let host = spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("spawn host");

        host.create_project(ProjectCreatePayload {
            name: "Code Intel Router".to_owned(),
            roots: vec![ProjectRootPath(project_root.to_string_lossy().into_owned())],
        })
        .await
        .expect("create project");
        let project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Code Intel Router")
                .expect("created project")
                .id
        };

        host.warm_code_intel_project(project_id.clone(), "test")
            .await
            .expect("warm code-intel project");
        {
            let state = host.state.lock().await;
            assert!(
                state.code_intel_routers.contains_key(&project_id),
                "warmup should create the per-project router"
            );
        }

        host.delete_project(ProjectDeletePayload {
            id: project_id.clone(),
        })
        .await
        .expect("delete project");

        let state = host.state.lock().await;
        assert!(
            !state.code_intel_routers.contains_key(&project_id),
            "project deletion must drop the code-intel router"
        );
    }

    #[tokio::test]
    async fn restarted_project_stream_recreates_code_intel_router_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_root = dir.path().join("project-root");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let host = spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("spawn host");

        host.create_project(ProjectCreatePayload {
            name: "Restarted Project Stream".to_owned(),
            roots: vec![ProjectRootPath(project_root.to_string_lossy().into_owned())],
        })
        .await
        .expect("create project");
        let project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Restarted Project Stream")
                .expect("created project")
                .id
        };

        host.warm_code_intel_project(project_id.clone(), "test")
            .await
            .expect("warm code-intel project");
        let old_handle = {
            let state = host.state.lock().await;
            let handle = state
                .project_streams
                .get(&project_id)
                .expect("project stream exists")
                .handle
                .clone();
            assert!(
                state
                    .code_intel_routers
                    .get(&project_id)
                    .expect("router exists")
                    .uses_project_handle_for_test(&handle),
                "router should start with the active project stream handle"
            );
            state
                .project_streams
                .get(&project_id)
                .expect("project stream exists")
                .task
                .abort();
            handle
        };

        for _ in 0..100 {
            let finished = {
                let state = host.state.lock().await;
                state
                    .project_streams
                    .get(&project_id)
                    .expect("project stream exists")
                    .task
                    .is_finished()
            };
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        {
            let state = host.state.lock().await;
            assert!(
                state
                    .project_streams
                    .get(&project_id)
                    .expect("project stream exists")
                    .task
                    .is_finished(),
                "aborted project stream should finish before restart"
            );
        }

        host.warm_code_intel_project(project_id.clone(), "test")
            .await
            .expect("warm code-intel project after stream restart");

        let state = host.state.lock().await;
        let new_handle = &state
            .project_streams
            .get(&project_id)
            .expect("project stream restarted")
            .handle;
        assert!(
            !new_handle.same_channel_for_test(&old_handle),
            "project stream restart should replace the handle"
        );
        assert!(
            state
                .code_intel_routers
                .get(&project_id)
                .expect("router recreated")
                .uses_project_handle_for_test(new_handle),
            "router must use the replacement project stream handle"
        );
    }

    #[test]
    fn dynamic_session_schema_unavailable_rejects_explicit_settings() {
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            protocol::SessionSettingValue::String("anthropic/claude-haiku-4.5".to_string()),
        );

        let failure =
            session_settings_startup_failure(BackendKind::Hermes, None, &settings, "supplied")
                .expect("non-empty settings without schema should fail");

        assert_eq!(failure.code, protocol::AgentErrorCode::BackendFailed);
        assert!(
            failure
                .message
                .contains("session settings schema unavailable"),
            "unexpected failure message: {}",
            failure.message
        );
    }

    #[test]
    fn dynamic_session_schema_unavailable_rejects_stored_settings() {
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "model".to_string(),
            protocol::SessionSettingValue::String("anthropic/claude-haiku-4.5".to_string()),
        );

        let (sanitized, failure) =
            sanitize_stored_session_settings(BackendKind::Hermes, None, Some(settings));

        assert!(sanitized.is_none());
        let failure = failure.expect("stored settings without schema should fail");
        assert_eq!(failure.code, protocol::AgentErrorCode::BackendFailed);
        assert!(
            failure
                .message
                .contains("session settings schema unavailable"),
            "unexpected failure message: {}",
            failure.message
        );
    }

    #[test]
    fn stored_session_settings_invalid_for_schema_are_rejected() {
        let mut settings = protocol::SessionSettingsValues::default();
        settings.0.insert(
            "default_agent".to_string(),
            protocol::SessionSettingValue::String("swarm".to_string()),
        );
        let schema = crate::backend::empty_session_settings_schema(BackendKind::Tycode);

        let (sanitized, failure) =
            sanitize_stored_session_settings(BackendKind::Tycode, Some(&schema), Some(settings));

        assert!(sanitized.is_none());
        let failure = failure.expect("invalid stored Tycode settings should fail");
        assert_eq!(failure.code, protocol::AgentErrorCode::Internal);
        assert!(
            failure.message.contains("invalid stored session settings"),
            "unexpected failure message: {}",
            failure.message
        );
        assert!(
            failure
                .message
                .contains("unknown session setting 'default_agent'"),
            "unexpected failure message: {}",
            failure.message
        );
    }

    #[test]
    fn worktree_path_sanitizes_branch_characters() {
        let parent = ProjectRootPath("/Users/mike/Tyde2".to_owned());

        let simple = compute_worktree_path(&parent, &GitBranchName("feature-login".to_owned()))
            .expect("compute simple worktree path");
        assert_eq!(simple.0, "/Users/mike/Tyde2--feature-login");

        let slash = compute_worktree_path(&parent, &GitBranchName("feature/login".to_owned()))
            .expect("compute slash worktree path");
        assert_eq!(slash.0, "/Users/mike/Tyde2--feature-login");

        // `%` in particular must never appear in the directory name:
        // rust-lld cannot write output files under a path containing `%`.
        let unicode = compute_worktree_path(&parent, &GitBranchName("café%".to_owned()))
            .expect("compute unicode worktree path");
        assert_eq!(unicode.0, "/Users/mike/Tyde2--caf--");
        assert!(!unicode.0.contains('%'));
    }

    #[tokio::test]
    async fn workbench_remove_reports_internal_when_parent_record_is_missing() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");
        let host = spawn_host_with_mock_backend(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
        )
        .expect("spawn host");

        let workbench = Project {
            id: ProjectId("workbench-test".to_owned()),
            name: "feature".to_owned(),
            sort_order: 0,
            source: ProjectSource::GitWorkbench {
                parent_project_id: ProjectId("missing-parent".to_owned()),
                branch: GitBranchName("feature".to_owned()),
                roots: vec![WorkbenchRoot {
                    parent_root: ProjectRootPath("/tmp/parent".to_owned()),
                    worktree_root: ProjectRootPath("/tmp/parent--feature".to_owned()),
                }],
            },
        };

        let error = host
            .validate_workbench_remove_blockers(&workbench)
            .await
            .expect_err("missing parent should block removal");
        assert_eq!(error.code(), protocol::CommandErrorCode::Internal);

        let _ = std::fs::remove_dir_all(&dir);
    }

    struct TeamFixture {
        _dir: tempfile::TempDir,
        host: HostHandle,
        team: protocol::Team,
        manager: TeamMember,
        report: TeamMember,
        custom_agent_id: CustomAgentId,
        project_id: ProjectId,
        project_root: String,
        agent_team_store_path: PathBuf,
    }

    struct TeamRaceFixture {
        temp_dir: tempfile::TempDir,
        host: HostHandle,
        team: protocol::Team,
        custom_agent_id: CustomAgentId,
        project_id: ProjectId,
        agent_team_store_path: PathBuf,
    }

    struct CompactFixture {
        _dir: tempfile::TempDir,
        host: HostHandle,
    }

    async fn team_fixture() -> TeamFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_root = dir.path().join("project-root");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let session_path = dir.path().join("sessions.json");
        let project_path = dir.path().join("projects.json");
        let settings_path = dir.path().join("settings.json");
        let agent_team_store_path = dir.path().join("agent_teams.json");
        let host = spawn_host_with_mock_backend(session_path, project_path, settings_path)
            .expect("spawn mock host");

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");

        let custom_agent_id = CustomAgentId(format!("custom-{}", Uuid::new_v4()));
        host.upsert_custom_agent(CustomAgentUpsertPayload {
            custom_agent: CustomAgent {
                id: custom_agent_id.clone(),
                name: "Team Custom Agent".to_owned(),
                description: "Handles team work".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        })
        .await
        .expect("upsert custom agent");

        host.create_project(ProjectCreatePayload {
            name: "Team Project".to_owned(),
            roots: vec![ProjectRootPath(project_root.to_string_lossy().to_string())],
        })
        .await
        .expect("create project");
        let project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Team Project")
                .expect("created project")
                .id
        };

        host.create_team(TeamCreatePayload {
            name: "Product Team".to_owned(),
            manager: TeamMemberCreateSpec {
                name: "Manager".to_owned(),
                description: "Coordinates reports".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent_id.clone()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![project_id.clone()],
            },
        })
        .await
        .expect("create team");
        let (team, manager) = {
            let snapshot = team_snapshot(&host).await;
            let team = snapshot
                .teams
                .into_iter()
                .find(|team| team.name == "Product Team")
                .expect("created team");
            let manager = snapshot
                .members
                .into_iter()
                .find(|member| member.id == team.manager_member_id)
                .expect("created manager");
            (team, manager)
        };

        host.create_team_member(TeamMemberCreatePayload {
            team_id: team.id.clone(),
            member: TeamMemberCreateSpec {
                name: "Report".to_owned(),
                description: "Implements delegated work".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent_id.clone()),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![project_id.clone()],
            },
            session_id: None,
        })
        .await
        .expect("create report");
        let report = team_snapshot(&host)
            .await
            .members
            .into_iter()
            .find(|member| member.team_id == team.id && member.role == TeamMemberRole::Report)
            .expect("created report");

        TeamFixture {
            _dir: dir,
            host,
            team,
            manager,
            report,
            custom_agent_id,
            project_id,
            project_root: project_root.to_string_lossy().to_string(),
            agent_team_store_path,
        }
    }

    async fn team_race_fixture() -> TeamRaceFixture {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let project_root = temp_dir.path().join("race-project-root");
        std::fs::create_dir_all(&project_root).expect("create project root");
        let project_root = project_root.to_string_lossy().to_string();
        let manager_project_root = temp_dir.path().join("race-manager-project-root");
        std::fs::create_dir_all(&manager_project_root).expect("create manager project root");
        let manager_project_root = manager_project_root.to_string_lossy().to_string();
        let session_path = temp_dir.path().join("sessions.json");
        let project_path = temp_dir.path().join("projects.json");
        let settings_path = temp_dir.path().join("settings.json");
        let agent_team_store_path = temp_dir.path().join("agent_teams.json");
        let host = spawn_host_with_mock_backend(session_path, project_path, settings_path)
            .expect("spawn mock host");

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");

        let manager_custom_agent_id = CustomAgentId(format!("manager-{}", Uuid::new_v4()));
        host.upsert_custom_agent(CustomAgentUpsertPayload {
            custom_agent: CustomAgent {
                id: manager_custom_agent_id.clone(),
                name: "Race Manager Custom Agent".to_owned(),
                description: "Owns the team manager".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        })
        .await
        .expect("upsert manager custom agent");

        let custom_agent_id = CustomAgentId(format!("race-{}", Uuid::new_v4()));
        host.upsert_custom_agent(CustomAgentUpsertPayload {
            custom_agent: CustomAgent {
                id: custom_agent_id.clone(),
                name: "Race Custom Agent".to_owned(),
                description: "The custom agent raced with member creation".to_owned(),
                instructions: None,
                skill_ids: Vec::new(),
                mcp_server_ids: Vec::new(),
                tool_policy: ToolPolicy::Unrestricted,
            },
        })
        .await
        .expect("upsert raced custom agent");

        host.create_project(ProjectCreatePayload {
            name: "Race Manager Project".to_owned(),
            roots: vec![ProjectRootPath(manager_project_root)],
        })
        .await
        .expect("create manager project");
        let manager_project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Race Manager Project")
                .expect("created manager project")
                .id
        };

        host.create_project(ProjectCreatePayload {
            name: "Race Project".to_owned(),
            roots: vec![ProjectRootPath(project_root.clone())],
        })
        .await
        .expect("create project");
        let project_id = {
            let state = host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Race Project")
                .expect("created project")
                .id
        };

        host.create_team(TeamCreatePayload {
            name: "Race Team".to_owned(),
            manager: TeamMemberCreateSpec {
                name: "Race Manager".to_owned(),
                description: "Coordinates the race test".to_owned(),
                profile: None,
                custom_agent_id: Some(manager_custom_agent_id),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids: vec![manager_project_id],
            },
        })
        .await
        .expect("create team");
        let team = team_snapshot(&host)
            .await
            .teams
            .into_iter()
            .find(|team| team.name == "Race Team")
            .expect("created team");

        TeamRaceFixture {
            temp_dir,
            host,
            team,
            custom_agent_id,
            project_id,
            agent_team_store_path,
        }
    }

    async fn team_snapshot(host: &HostHandle) -> crate::team_registry::TeamRegistrySnapshot {
        let registry = { host.state.lock().await.team_registry.clone() };
        registry.snapshot().await.expect("team snapshot")
    }

    async fn bind_team_member(host: &HostHandle, member: &TeamMember) -> AgentId {
        let agent_id = AgentId(Uuid::new_v4().to_string());
        let session_id = SessionId(format!("session-{}", Uuid::new_v4()));
        let (registry, refs) = {
            let state = host.state.lock().await;
            (
                state.team_registry.clone(),
                agent_team_validation_refs(&state, "test_bind_team_member")
                    .await
                    .expect("team refs"),
            )
        };
        let events = registry
            .bind_member_agent(member.id.clone(), agent_id.clone(), Some(session_id), refs)
            .await
            .expect("bind member");
        host.fan_out_team_registry_events(events).await;
        agent_id
    }

    fn member_from_snapshot(
        host_snapshot: crate::team_registry::TeamRegistrySnapshot,
        id: &TeamMemberId,
    ) -> TeamMember {
        host_snapshot
            .members
            .into_iter()
            .find(|member| member.id == *id)
            .expect("member in snapshot")
    }

    fn persisted_team_store(path: &Path) -> AgentTeamsStoreFile {
        let json = std::fs::read_to_string(path).expect("read agent teams store");
        serde_json::from_str(&json).expect("parse agent teams store")
    }

    fn assert_no_team_member_references_project(
        store: &AgentTeamsStoreFile,
        project_id: &ProjectId,
    ) {
        for member in store.members.values() {
            assert!(
                !member.project_ids.contains(project_id),
                "member {} still references deleted project {}",
                member.id,
                project_id
            );
        }
    }

    async fn assert_agent_team_store_loads_with_current_refs(host: &HostHandle, path: &Path) {
        let refs = {
            let state = host.state.lock().await;
            agent_team_validation_refs(&state, "test_validate_agent_team_store")
                .await
                .expect("team refs")
        };
        AgentTeamsStore::load(path.to_path_buf(), &refs).expect("agent teams store validates");
    }

    fn team_member_create_payload(
        fixture: &TeamRaceFixture,
        custom_agent_id: CustomAgentId,
        project_ids: Vec<ProjectId>,
    ) -> TeamMemberCreatePayload {
        TeamMemberCreatePayload {
            team_id: fixture.team.id.clone(),
            member: TeamMemberCreateSpec {
                name: "Race Report".to_owned(),
                description: "Created while a referenced record is deleting".to_owned(),
                profile: None,
                custom_agent_id: Some(custom_agent_id),
                backend_kind: BackendKind::Claude,
                cost_hint: None,
                project_ids,
            },
            session_id: None,
        }
    }

    fn team_mutation_race_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    async fn wait_for_team_member_unbound(host: &HostHandle, member_id: &TeamMemberId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = team_snapshot(host).await;
            let binding = snapshot
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .expect("member binding");
            if binding.current_agent_id.is_none() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for team member {member_id} to unbind"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_team_member_binding_idle(host: &HostHandle, member_id: &TeamMemberId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = team_snapshot(host).await;
            let binding = snapshot
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .expect("member binding");
            if binding.current_agent_id.is_some() && binding.status == AgentControlStatus::Idle {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for team member {member_id} binding to become idle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn compact_fixture() -> CompactFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("spawn mock host");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");
        CompactFixture { _dir: dir, host }
    }

    async fn compact_fixture_without_supervisor_worker() -> CompactFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = spawn_host_with_mock_backend_and_runtime_config(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
            HostRuntimeConfig {
                skip_real_backend_probe: true,
                start_agent_supervisor_worker: false,
                ..HostRuntimeConfig::default()
            },
        )
        .expect("spawn mock host without supervisor worker");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Claude],
            },
        })
        .await
        .expect("enable backend");
        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Claude),
            },
        })
        .await
        .expect("set default backend");
        CompactFixture { _dir: dir, host }
    }

    fn task_usage_amount(input_tokens: u64, output_tokens: u64) -> TaskTokenUsageAmount {
        TaskTokenUsageAmount {
            total_tokens: input_tokens + output_tokens,
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn task_usage_start(
        agent_id: AgentId,
        parent_agent_id: Option<AgentId>,
        created_at_ms: u64,
    ) -> AgentStartPayload {
        AgentStartPayload {
            agent_id,
            name: "Task Usage Agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            workspace_roots: Vec::new(),
            custom_agent_id: None,
            team_id: None,
            team_member_id: None,
            project_id: None,
            parent_agent_id,
            session_id: None,
            workflow: None,
            created_at_ms,
        }
    }

    #[test]
    fn task_token_usage_rollup_preserves_partial_root_self_usage() {
        let root_id = AgentId("partial-root".to_owned());
        let snapshots = vec![AgentUsageSnapshot {
            start: task_usage_start(root_id.clone(), None, 1),
            usage: TaskTokenUsageScope::Partial {
                usage: Box::new(task_usage_amount(30, 12)),
                unavailable_count: 1,
                reasons: vec![TaskTokenUsageUnavailableReason::BackendDidNotReport],
            },
            model: Some("mock".to_owned()),
        }];
        let live_agent_ids = HashSet::from([root_id.clone()]);
        let agent_sessions = HashMap::new();

        let payloads =
            task_token_usage_rollups_from_snapshots(snapshots, &live_agent_ids, &agent_sessions);

        assert_eq!(payloads.len(), 1);
        let payload = &payloads[0];
        assert_eq!(payload.root_agent_id, root_id);
        assert_eq!(payload.total.usage.total_tokens, 42);
        assert_eq!(payload.total.usage.input_tokens, Some(30));
        assert_eq!(payload.total.usage.output_tokens, Some(12));
        assert!(matches!(
            payload.total.status,
            TaskTokenUsageStatus::Partial {
                unavailable_count: 1,
                ref reasons
            } if reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
        ));
        assert!(matches!(
            payload.self_usage,
            TaskTokenUsageScope::Partial {
                ref usage,
                unavailable_count: 1,
                ref reasons
            } if usage.total_tokens == 42
                && reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
        ));
        assert_eq!(payload.breakdown.len(), 1);
        assert!(matches!(
            payload.breakdown[0].usage,
            TaskTokenUsageScope::Partial {
                ref usage,
                unavailable_count: 1,
                ref reasons
            } if usage.total_tokens == 42
                && reasons == &vec![TaskTokenUsageUnavailableReason::BackendDidNotReport]
        ));
    }

    async fn spawn_idle_user_agent(host: &HostHandle, prompt: &str) -> (AgentId, SessionId) {
        let agent_id = host
            .spawn_agent(SpawnAgentPayload {
                name: Some("Compact Me".to_owned()),
                custom_agent_id: None,
                parent_agent_id: None,
                project_id: None,
                params: SpawnAgentParams::New {
                    workspace_roots: Vec::new(),
                    prompt: prompt.to_owned(),
                    images: None,
                    backend_kind: BackendKind::Claude,
                    launch_profile_id: None,
                    cost_hint: None,
                    access_mode: Default::default(),
                    session_settings: None,
                },
            })
            .await
            .expect("spawn idle user agent");
        let session_id = host
            .wait_for_agent_session_id_result(&agent_id)
            .await
            .expect("agent session id");
        wait_for_agent_idle(host, &agent_id).await;
        (agent_id, session_id)
    }

    #[tokio::test]
    async fn lazy_host_registration_defers_agent_bootstrap_until_load() {
        let fixture = compact_fixture().await;
        let (agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember lazy mobile startup").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/lazy-agents-{}", Uuid::new_v4()));
        let host_stream = Stream::new(host_path.clone(), tx);

        assert!(
            fixture
                .host
                .register_host_stream(host_stream.clone(), AgentReplayMode::Lazy)
                .await
                .is_empty()
        );

        let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("lazy registration should emit HostBootstrap")
            .expect("output envelope");
        assert_eq!(envelope.kind, FrameKind::HostBootstrap);
        let bootstrap: HostBootstrapPayload = envelope.parse_payload().expect("host bootstrap");
        let agent_stream = bootstrap
            .agents
            .iter()
            .find_map(|agent| (agent.agent_id == agent_id).then_some(agent.instance_stream.clone()))
            .expect("existing agent advertised in HostBootstrap");

        let agent_replay = tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                let envelope = rx.recv().await?;
                if envelope.stream == agent_stream {
                    return Some(envelope);
                }
            }
        })
        .await;
        assert!(
            agent_replay.is_err(),
            "lazy registration must not replay agent transcripts until requested"
        );

        let load = protocol::Envelope::from_payload(
            agent_stream.clone(),
            FrameKind::LoadAgent,
            0,
            &protocol::LoadAgentPayload {},
        )
        .expect("load agent envelope");
        crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, load)
            .await
            .expect("route load_agent");

        let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("load_agent should emit AgentBootstrap")
            .expect("output envelope");
        assert_eq!(envelope.stream, agent_stream);
        assert_eq!(envelope.kind, FrameKind::AgentBootstrap);
        let bootstrap: protocol::AgentBootstrapPayload =
            envelope.parse_payload().expect("agent bootstrap");
        assert!(
            bootstrap
                .events
                .iter()
                .any(|event| matches!(event, protocol::AgentBootstrapEvent::AgentStart(_))),
            "loaded agent bootstrap should include its AgentStart snapshot"
        );
    }

    #[tokio::test]
    async fn task_token_usage_keeps_unresponsive_live_agent_unavailable() {
        let fixture = compact_fixture().await;
        let (agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember unavailable live agent").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/unavailable-agent-{}", Uuid::new_v4()));
        let host_stream = Stream::new(host_path, tx);

        assert!(
            fixture
                .host
                .register_host_stream(host_stream, AgentReplayMode::Lazy)
                .await
                .is_empty()
        );
        let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("registration should emit HostBootstrap")
            .expect("output envelope");
        assert_eq!(envelope.kind, FrameKind::HostBootstrap);

        let handle = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_handle(&agent_id)
                .expect("live registry handle")
        };
        assert!(handle.close().await);

        fixture.host.fan_out_task_token_usages().await;

        let payload = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("task token usage should be emitted")
                .expect("output envelope");
            if envelope.kind != FrameKind::TaskTokenUsage {
                continue;
            }
            let payload: TaskTokenUsagePayload =
                envelope.parse_payload().expect("TaskTokenUsage payload");
            if payload.root_agent_id == agent_id {
                break payload;
            }
        };

        assert_eq!(payload.descendant_count, 0);
        assert_eq!(payload.breakdown.len(), 1);
        assert_eq!(payload.breakdown[0].agent_id, agent_id);
        assert!(matches!(
            payload.self_usage,
            TaskTokenUsageScope::Unavailable {
                reason: TaskTokenUsageUnavailableReason::AgentUnavailable
            }
        ));
        assert_eq!(payload.total.usage.total_tokens, 0);
        assert_eq!(payload.total.usage.input_tokens, None);
        assert!(matches!(
            payload.total.status,
            TaskTokenUsageStatus::Unavailable {
                unavailable_count: 1,
                ref reasons
            } if reasons == &vec![TaskTokenUsageUnavailableReason::AgentUnavailable]
        ));
    }

    #[tokio::test]
    async fn bootstrapping_new_agent_fanout_is_deferred_until_after_bootstrap() {
        let fixture = compact_fixture().await;
        let (agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember bootstrap fanout ordering").await;
        let handle = {
            let state = fixture.host.state.lock().await;
            state
                .registry
                .agent_handle(&agent_id)
                .expect("live registry handle")
        };
        let start = handle.snapshot();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/bootstrap-pending-{}", Uuid::new_v4()));
        let stream = Stream::new(host_path, tx);
        let mut subscriber = HostSubscriber {
            stream: stream.clone(),
            bootstrapped: false,
            agent_replay: AgentReplayMode::Eager,
            session_list_replay: SessionListReplayMode::Full,
            session_list_snapshot: None,
            known_agent_streams: HashSet::new(),
            attached_agent_streams: HashSet::new(),
            bootstrapped_agent_streams: HashSet::new(),
            pending_bootstrap_new_agents: Vec::new(),
            pending_bootstrap_frames: Vec::new(),
            last_session_schemas: None,
            last_backend_config_schemas: None,
            last_backend_config_snapshots: None,
            last_backend_native_settings_snapshots: None,
            last_backend_capacity: None,
            capacity_replay_ready: false,
            last_launch_profile_catalog: None,
        };

        assert!(
            prepare_new_agent_fanout_for_subscriber(
                &mut subscriber,
                &start,
                &handle,
                AgentActivitySummaryState::default(),
            )
            .is_none()
        );
        assert_eq!(subscriber.pending_bootstrap_new_agents.len(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "NewAgent must not be emitted before HostBootstrap"
        );

        subscriber
            .stream
            .send_value(FrameKind::HostBootstrap, serde_json::json!({}))
            .expect("send bootstrap marker");
        subscriber.bootstrapped = true;
        for pending in std::mem::take(&mut subscriber.pending_bootstrap_new_agents) {
            emit_new_agent_for_stream(
                &pending.start,
                &pending.agent_handle,
                &subscriber.stream,
                pending.instance_stream,
                pending.attach_eagerly,
                pending.activity_summary,
            )
            .expect("flush pending NewAgent");
        }

        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("bootstrap marker should be first")
            .expect("bootstrap marker envelope");
        assert_eq!(first.kind, FrameKind::HostBootstrap);
        let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("pending NewAgent should follow bootstrap")
            .expect("pending NewAgent envelope");
        assert_eq!(second.kind, FrameKind::NewAgent);
        let payload: NewAgentPayload = second.parse_payload().expect("NewAgent payload");
        assert_eq!(payload.agent_id, agent_id);
    }

    #[tokio::test]
    async fn forced_backend_config_snapshot_fanout_reemits_unchanged_native_settings() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/backend-config-{}", Uuid::new_v4()));
        let stream = Stream::new(host_path, tx);
        let mut subscriber = HostSubscriber {
            stream,
            bootstrapped: true,
            agent_replay: AgentReplayMode::Eager,
            session_list_replay: SessionListReplayMode::Full,
            session_list_snapshot: None,
            known_agent_streams: HashSet::new(),
            attached_agent_streams: HashSet::new(),
            bootstrapped_agent_streams: HashSet::new(),
            pending_bootstrap_new_agents: Vec::new(),
            pending_bootstrap_frames: Vec::new(),
            last_session_schemas: None,
            last_backend_config_schemas: None,
            last_backend_config_snapshots: None,
            last_backend_native_settings_snapshots: None,
            last_backend_capacity: None,
            capacity_replay_ready: true,
            last_launch_profile_catalog: None,
        };
        let native_settings = vec![BackendNativeSettingsSnapshot {
            backend_kind: BackendKind::Tycode,
            status: BackendConfigSnapshotStatus::Ready,
            settings: Some(serde_json::json!({
                "active_provider": "default",
                "model_quality": "high",
            })),
            groups: Vec::new(),
            message: None,
            advisories: Vec::new(),
        }];

        emit_backend_config_snapshots_for_subscriber(&[], &native_settings, &mut subscriber, false)
            .await
            .expect("initial backend config snapshot fanout");
        let first = rx.recv().await.expect("initial snapshot event");
        assert_eq!(first.kind, FrameKind::BackendConfigSnapshots);

        emit_backend_config_snapshots_for_subscriber(&[], &native_settings, &mut subscriber, false)
            .await
            .expect("unchanged backend config snapshot fanout");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "unchanged snapshots should still be deduped during ordinary refresh"
        );

        emit_backend_config_snapshots_for_subscriber(&[], &native_settings, &mut subscriber, true)
            .await
            .expect("forced backend config snapshot fanout");
        let forced = rx.recv().await.expect("forced snapshot event");
        assert_eq!(forced.kind, FrameKind::BackendConfigSnapshots);
        let payload: BackendConfigSnapshotsPayload = forced
            .parse_payload()
            .expect("forced BackendConfigSnapshots payload");
        assert_eq!(payload.native_settings, native_settings);
    }

    #[test]
    fn backend_setup_refresh_order_is_authoritative() {
        assert_eq!(
            BACKEND_SETUP_REFRESH_ORDER,
            [
                BackendSetupRefreshStep::Setup,
                BackendSetupRefreshStep::SessionSchemas,
                BackendSetupRefreshStep::BackendConfigSnapshots,
            ]
        );
    }

    #[tokio::test]
    async fn forced_session_schema_fanout_reemits_unchanged_snapshot() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/session-schema-{}", Uuid::new_v4()));
        let stream = Stream::new(host_path, tx);
        let mut subscriber = HostSubscriber {
            stream,
            bootstrapped: true,
            agent_replay: AgentReplayMode::Eager,
            session_list_replay: SessionListReplayMode::Full,
            session_list_snapshot: None,
            known_agent_streams: HashSet::new(),
            attached_agent_streams: HashSet::new(),
            bootstrapped_agent_streams: HashSet::new(),
            pending_bootstrap_new_agents: Vec::new(),
            pending_bootstrap_frames: Vec::new(),
            last_session_schemas: None,
            last_backend_config_schemas: None,
            last_backend_config_snapshots: None,
            last_backend_native_settings_snapshots: None,
            last_backend_capacity: None,
            capacity_replay_ready: true,
            last_launch_profile_catalog: None,
        };

        emit_session_schemas_for_subscriber(&[], &mut subscriber, false)
            .await
            .expect("initial session schema fanout");
        let first = rx.recv().await.expect("initial session schema event");
        assert_eq!(first.kind, FrameKind::SessionSchemas);

        emit_session_schemas_for_subscriber(&[], &mut subscriber, false)
            .await
            .expect("unchanged session schema fanout");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "ordinary refresh should still dedupe unchanged session schemas"
        );

        emit_session_schemas_for_subscriber(&[], &mut subscriber, true)
            .await
            .expect("forced session schema fanout");
        let forced = rx.recv().await.expect("forced session schema event");
        assert_eq!(forced.kind, FrameKind::SessionSchemas);
    }

    async fn wait_for_agent_idle(host: &HostHandle, agent_id: &AgentId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = host.agent_status_snapshot(agent_id).await
                && status.started
                && !status.terminated
                && !status.is_active()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for agent {agent_id} to become idle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_agent_active(host: &HostHandle, agent_id: &AgentId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = host.agent_status_snapshot(agent_id).await
                && status.started
                && !status.terminated
                && status.is_active()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for agent {agent_id} to become active"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn compact_stream(agent_id: &AgentId) -> (Stream, mpsc::UnboundedReceiver<protocol::Envelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Stream::new(StreamPath(format!("/agent/{}", agent_id)), tx),
            rx,
        )
    }

    fn drain_validated_events(
        validator: &mut ProtocolValidator,
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
        context: &str,
    ) -> Vec<protocol::Envelope> {
        let mut envelopes = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            validator
                .validate_envelope(&envelope)
                .unwrap_or_else(|err| {
                    panic!(
                        "{context}: protocol violation after {} on {}: {err}",
                        envelope.kind, envelope.stream
                    )
                });
            envelopes.push(envelope);
        }
        envelopes
    }

    fn drain_compact_notifies(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
    ) -> Vec<AgentCompactNotifyPayload> {
        let mut notifies = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            if envelope.kind == FrameKind::AgentCompactNotify {
                notifies.push(
                    envelope
                        .parse_payload::<AgentCompactNotifyPayload>()
                        .expect("compact notify payload"),
                );
            }
        }
        notifies
    }

    #[tokio::test]
    async fn agent_compaction_route_does_not_block_host_commands() {
        let fixture = compact_fixture().await;
        let (old_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember routing responsiveness").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/compact-route-{}", Uuid::new_v4()));
        let host_stream = Stream::new(host_path.clone(), tx);
        assert!(
            fixture
                .host
                .register_host_stream(host_stream.clone(), AgentReplayMode::Lazy)
                .await
                .is_empty()
        );
        let agent_path = StreamPath(format!("/agent/{}/{}", old_agent_id, Uuid::new_v4()));
        let compact = protocol::Envelope::from_payload(
            agent_path,
            FrameKind::AgentCompact,
            0,
            &AgentCompactPayload {
                summary_prompt: Some(format!("{MOCK_SLOW_TURN_SENTINEL} summarize")),
                max_summary_bytes: None,
            },
        )
        .expect("compact envelope");

        tokio::time::timeout(
            Duration::from_millis(200),
            crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, compact),
        )
        .await
        .expect("agent_compact route should return without waiting for compaction")
        .expect("route compact");

        let list_sessions = protocol::Envelope::from_payload(
            host_path.clone(),
            FrameKind::ListSessions,
            0,
            &protocol::ListSessionsPayload::default(),
        )
        .expect("list sessions envelope");
        tokio::time::timeout(
            Duration::from_millis(200),
            crate::router::route_client_envelope(
                &fixture.host,
                &host_path,
                &host_stream,
                list_sessions,
            ),
        )
        .await
        .expect("list_sessions route should not wait behind compaction")
        .expect("route list sessions");

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("session list should be emitted while compaction is still running")
                .expect("output envelope");
            if envelope.kind == FrameKind::SessionList {
                break;
            }
            if envelope.kind == FrameKind::AgentCompactNotify {
                let payload: AgentCompactNotifyPayload =
                    envelope.parse_payload().expect("compact notify");
                assert_ne!(
                    payload.status,
                    AgentCompactStatus::Completed,
                    "slow compaction completed before ListSessions was processed"
                );
            }
        }

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("background compaction should finish")
                .expect("output envelope");
            if envelope.kind != FrameKind::AgentCompactNotify {
                continue;
            }
            let payload: AgentCompactNotifyPayload =
                envelope.parse_payload().expect("compact notify");
            if matches!(
                payload.status,
                AgentCompactStatus::Completed | AgentCompactStatus::Failed
            ) {
                assert_eq!(payload.status, AgentCompactStatus::Completed);
                break;
            }
        }
    }

    #[tokio::test]
    async fn agent_compaction_route_orders_later_agent_input_after_compact() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_path = StreamPath(format!("/host/compact-ordering-{}", Uuid::new_v4()));
        let host_stream = Stream::new(host_path.clone(), tx);
        assert!(
            fixture
                .host
                .register_host_stream(host_stream.clone(), AgentReplayMode::Eager)
                .await
                .is_empty()
        );

        let (old_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember input ordering").await;
        let old_instance_stream = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("new agent should be emitted")
                .expect("output envelope");
            if envelope.kind != FrameKind::NewAgent {
                continue;
            }
            let payload: NewAgentPayload = envelope.parse_payload().expect("new agent payload");
            if payload.agent_id == old_agent_id {
                break payload.instance_stream;
            }
        };

        let compact = protocol::Envelope::from_payload(
            old_instance_stream.clone(),
            FrameKind::AgentCompact,
            0,
            &AgentCompactPayload {
                summary_prompt: Some(format!("{MOCK_SLOW_TURN_SENTINEL} summarize")),
                max_summary_bytes: None,
            },
        )
        .expect("compact envelope");
        crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, compact)
            .await
            .expect("route compact");

        let send = protocol::Envelope::from_payload(
            old_instance_stream,
            FrameKind::SendMessage,
            1,
            &SendMessagePayload {
                message: "This input must wait behind compaction".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        )
        .expect("send envelope");
        crate::router::route_client_envelope(&fixture.host, &host_path, &host_stream, send)
            .await
            .expect("route send");

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("compacting agent should reject later input")
                .expect("output envelope");
            if envelope.kind != FrameKind::AgentError {
                continue;
            }
            let payload: AgentErrorPayload = envelope.parse_payload().expect("agent error payload");
            if payload.agent_id == old_agent_id
                && payload.message.contains("compaction is in progress")
            {
                break;
            }
        }

        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("background compaction should finish")
                .expect("output envelope");
            if envelope.kind != FrameKind::AgentCompactNotify {
                continue;
            }
            let payload: AgentCompactNotifyPayload =
                envelope.parse_payload().expect("compact notify");
            if matches!(
                payload.status,
                AgentCompactStatus::Completed | AgentCompactStatus::Failed
            ) {
                assert_eq!(payload.status, AgentCompactStatus::Completed);
                break;
            }
        }
    }

    #[tokio::test]
    async fn agent_compaction_rotates_user_agent() {
        let fixture = compact_fixture().await;
        let (old_agent_id, old_session_id) =
            spawn_idle_user_agent(&fixture.host, "remember this user preference").await;
        let (stream, mut rx) = compact_stream(&old_agent_id);

        fixture
            .host
            .compact_agent(old_agent_id.clone(), AgentCompactPayload::default(), stream)
            .await
            .expect("compact agent");

        let notifies = drain_compact_notifies(&mut rx);
        assert_eq!(
            notifies.first().map(|notify| notify.status),
            Some(AgentCompactStatus::Started)
        );
        let completed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Completed)
            .expect("completed notify");
        let new_agent_id = completed
            .new_agent_id
            .clone()
            .expect("new agent id in completed notify");
        let new_session_id = completed
            .new_session_id
            .clone()
            .expect("new session id in completed notify");
        assert_ne!(new_agent_id, old_agent_id);
        assert_ne!(new_session_id, old_session_id);
        assert!(fixture.host.agent_handle(&old_agent_id).await.is_none());
        assert!(fixture.host.agent_handle(&new_agent_id).await.is_some());

        let (old_record, new_record) = {
            let state = fixture.host.state.lock().await;
            let store = state.session_store.lock().await;
            (
                store.get(&old_session_id).expect("old record"),
                store.get(&new_session_id).expect("new record"),
            )
        };
        assert!(!old_record.resumable);
        assert_eq!(
            old_record.compacted_to_session_id.as_ref(),
            Some(&new_session_id)
        );
        assert_eq!(
            new_record.compacted_from_session_id.as_ref(),
            Some(&old_session_id)
        );
        assert!(
            old_record
                .compaction_summary_preview
                .as_deref()
                .is_some_and(|preview| preview.contains("mock backend response"))
        );
    }

    #[tokio::test]
    async fn agent_compaction_completed_validates_before_old_agent_closed_on_instance_stream() {
        let fixture = compact_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let host_stream = Stream::new(
            StreamPath(format!("/host/compact-order-{}", Uuid::new_v4())),
            tx.clone(),
        );
        assert!(
            fixture
                .host
                .register_host_stream(host_stream, AgentReplayMode::Eager)
                .await
                .is_empty()
        );
        let mut validator = ProtocolValidator::new();
        drain_validated_events(&mut validator, &mut rx, "host registration");

        let (old_agent_id, _) =
            spawn_idle_user_agent(&fixture.host, "remember protocol ordering").await;
        let spawn_events = drain_validated_events(&mut validator, &mut rx, "old agent spawn");
        let old_instance_stream = spawn_events
            .iter()
            .find_map(|envelope| {
                if envelope.kind != FrameKind::NewAgent {
                    return None;
                }
                let payload: NewAgentPayload =
                    envelope.parse_payload().expect("parse old NewAgent");
                (payload.agent_id == old_agent_id).then_some(payload.instance_stream)
            })
            .expect("old agent instance stream");
        assert!(
            old_instance_stream
                .0
                .starts_with(&format!("/agent/{}/", old_agent_id))
        );

        let compact_stream = Stream::new(old_instance_stream.clone(), tx);
        fixture
            .host
            .compact_agent(
                old_agent_id.clone(),
                AgentCompactPayload::default(),
                compact_stream,
            )
            .await
            .expect("compact agent");

        let compact_events = drain_validated_events(&mut validator, &mut rx, "agent compaction");
        let mut completed_index = None;
        let mut closed_index = None;
        for (index, envelope) in compact_events.iter().enumerate() {
            match envelope.kind {
                FrameKind::AgentCompactNotify if envelope.stream == old_instance_stream => {
                    let payload: AgentCompactNotifyPayload =
                        envelope.parse_payload().expect("compact notify");
                    if payload.status == AgentCompactStatus::Completed {
                        completed_index = Some(index);
                    }
                }
                FrameKind::AgentClosed => {
                    let payload: AgentClosedPayload =
                        envelope.parse_payload().expect("agent closed");
                    if payload.agent_id == old_agent_id {
                        closed_index = Some(index);
                    }
                }
                _ => {}
            }
        }
        let completed_index = completed_index.expect("completed compact notify");
        let closed_index = closed_index.expect("old AgentClosed");
        assert!(
            completed_index < closed_index,
            "Completed notify must arrive before AgentClosed invalidates the old stream"
        );
    }

    #[tokio::test]
    async fn agent_compaction_rejects_busy_agent() {
        let fixture = compact_fixture().await;
        let resolved = ResolvedSpawnConfig {
            instructions: Some(MOCK_SLOW_TURN_SENTINEL.to_owned()),
            ..Default::default()
        };
        let agent_id = fixture
            .host
            .spawn_agent_with_origin_config_and_team(
                SpawnAgentPayload {
                    name: Some("Busy Agent".to_owned()),
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: SpawnAgentParams::New {
                        workspace_roots: Vec::new(),
                        prompt: "slow start".to_owned(),
                        images: None,
                        backend_kind: BackendKind::Claude,
                        launch_profile_id: None,
                        cost_hint: None,
                        access_mode: Default::default(),
                        session_settings: None,
                    },
                },
                AgentOrigin::User,
                Some(resolved),
                None,
                None,
                None,
            )
            .await
            .expect("spawn busy agent");
        let old_session_id = fixture
            .host
            .wait_for_agent_session_id_result(&agent_id)
            .await
            .expect("agent session id");
        wait_for_agent_active(&fixture.host, &agent_id).await;
        let (stream, mut rx) = compact_stream(&agent_id);

        fixture
            .host
            .compact_agent(agent_id.clone(), AgentCompactPayload::default(), stream)
            .await
            .expect("compact busy agent");

        let notifies = drain_compact_notifies(&mut rx);
        let failed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Failed)
            .expect("failed notify");
        assert!(
            failed
                .message
                .as_deref()
                .is_some_and(|message| message.contains("busy"))
        );
        assert!(fixture.host.agent_handle(&agent_id).await.is_some());
        let old_record = {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .get(&old_session_id)
                .expect("old record")
        };
        assert!(old_record.resumable);
        assert!(old_record.compacted_to_session_id.is_none());
    }

    #[tokio::test]
    async fn agent_compaction_summary_failure_leaves_old_agent() {
        let fixture = compact_fixture().await;
        let (old_agent_id, old_session_id) =
            spawn_idle_user_agent(&fixture.host, "initial prompt").await;
        let (stream, mut rx) = compact_stream(&old_agent_id);

        fixture
            .host
            .compact_agent(
                old_agent_id.clone(),
                AgentCompactPayload {
                    summary_prompt: Some("/compact".to_owned()),
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("compact agent");

        let notifies = drain_compact_notifies(&mut rx);
        let failed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Failed)
            .expect("failed notify");
        assert!(
            failed
                .message
                .as_deref()
                .is_some_and(|message| message.contains("summary was empty"))
        );
        assert!(fixture.host.agent_handle(&old_agent_id).await.is_some());
        let old_record = {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .get(&old_session_id)
                .expect("old record")
        };
        assert!(old_record.resumable);
        assert!(old_record.compacted_to_session_id.is_none());
    }

    #[tokio::test]
    async fn agent_compaction_replacement_failure_leaves_old_agent() {
        let fixture = compact_fixture().await;
        let (old_agent_id, old_session_id) =
            spawn_idle_user_agent(&fixture.host, "initial prompt").await;
        let (stream, mut rx) = compact_stream(&old_agent_id);

        fixture
            .host
            .compact_agent(
                old_agent_id.clone(),
                AgentCompactPayload {
                    summary_prompt: Some("__mock_fail_spawn__".to_owned()),
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("compact agent");

        let notifies = drain_compact_notifies(&mut rx);
        let failed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Failed)
            .expect("failed notify");
        assert!(
            failed
                .message
                .as_deref()
                .is_some_and(|message| message.contains("replacement agent failed"))
        );
        assert!(fixture.host.agent_handle(&old_agent_id).await.is_some());
        if let Some(new_agent_id) = failed.new_agent_id.as_ref() {
            assert!(fixture.host.agent_handle(new_agent_id).await.is_none());
        }
        let old_record = {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .get(&old_session_id)
                .expect("old record")
        };
        assert!(old_record.resumable);
        assert!(old_record.compacted_to_session_id.is_none());
    }

    #[tokio::test]
    async fn agent_compaction_rotates_team_member_session() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;
        let outcome = fixture
            .host
            .message_team_member(
                manager_agent_id.clone(),
                fixture.report.id.clone(),
                "Please investigate the bug".to_owned(),
                None,
            )
            .await
            .expect("message report");
        let old_report_agent_id = outcome.agent_id;
        let old_report_session_id = fixture
            .host
            .wait_for_agent_session_id_result(&old_report_agent_id)
            .await
            .expect("report session");
        wait_for_agent_idle(&fixture.host, &old_report_agent_id).await;
        let (stream, mut rx) = compact_stream(&old_report_agent_id);

        fixture
            .host
            .compact_agent(
                old_report_agent_id.clone(),
                AgentCompactPayload::default(),
                stream,
            )
            .await
            .expect("compact report");

        let notifies = drain_compact_notifies(&mut rx);
        let completed = notifies
            .iter()
            .find(|notify| notify.status == AgentCompactStatus::Completed)
            .expect("completed notify");
        let replacement_agent_id = completed
            .new_agent_id
            .clone()
            .expect("replacement agent id");
        let replacement_session_id = completed
            .new_session_id
            .clone()
            .expect("replacement session id");
        assert_ne!(replacement_session_id, old_report_session_id);

        let snapshot = team_snapshot(&fixture.host).await;
        let report = member_from_snapshot(snapshot.clone(), &fixture.report.id);
        assert_eq!(report.session_id.as_ref(), Some(&replacement_session_id));
        let binding = snapshot
            .bindings
            .iter()
            .find(|binding| binding.member_id == fixture.report.id)
            .expect("report binding");
        assert_eq!(
            binding.current_agent_id.as_ref(),
            Some(&replacement_agent_id)
        );

        assert!(fixture.host.close_agent(&replacement_agent_id).await);
        wait_for_team_member_unbound(&fixture.host, &fixture.report.id).await;
        let resumed = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Follow up after compaction".to_owned(),
                None,
            )
            .await
            .expect("message compacted report");
        let resumed_session_id = fixture
            .host
            .wait_for_agent_session_id_result(&resumed.agent_id)
            .await
            .expect("resumed report session");
        assert_eq!(resumed_session_id, replacement_session_id);
    }

    #[tokio::test]
    async fn team_compaction_rejects_busy_team() {
        let fixture = team_fixture().await;
        let manager = fixture
            .host
            .activate_team_member(
                fixture.manager.id.clone(),
                Some(format!("{MOCK_SLOW_TURN_SENTINEL} start manager")),
                None,
            )
            .await
            .expect("activate busy manager");
        wait_for_agent_active(&fixture.host, &manager.agent_id).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let stream = Stream::new(
            StreamPath(format!("/host/team-compact-busy-{}", Uuid::new_v4())),
            tx,
        );

        let error = fixture
            .host
            .compact_team(
                TeamCompactPayload {
                    team_id: fixture.team.id.clone(),
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect_err("busy team should reject compaction");
        assert!(
            error.message.contains("not idle"),
            "unexpected error: {error}"
        );
        wait_for_agent_idle(&fixture.host, &manager.agent_id).await;
    }

    #[tokio::test]
    async fn team_compaction_rotates_live_idle_members() {
        let fixture = team_fixture().await;
        let manager = fixture
            .host
            .activate_team_member(
                fixture.manager.id.clone(),
                Some("Start the manager".to_owned()),
                None,
            )
            .await
            .expect("activate manager");
        wait_for_agent_idle(&fixture.host, &manager.agent_id).await;
        wait_for_team_member_binding_idle(&fixture.host, &fixture.manager.id).await;

        let report = fixture
            .host
            .message_team_member(
                manager.agent_id.clone(),
                fixture.report.id.clone(),
                "Please investigate the bug".to_owned(),
                None,
            )
            .await
            .expect("message report");
        wait_for_agent_idle(&fixture.host, &report.agent_id).await;
        wait_for_team_member_binding_idle(&fixture.host, &fixture.report.id).await;
        let old_agent_ids = HashSet::from([manager.agent_id.clone(), report.agent_id.clone()]);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(
            StreamPath(format!("/host/team-compact-{}", Uuid::new_v4())),
            tx,
        );
        fixture
            .host
            .compact_team(
                TeamCompactPayload {
                    team_id: fixture.team.id.clone(),
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("compact team");

        let started = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("team compaction started notify")
            .expect("team compaction started envelope");
        assert_eq!(started.kind, FrameKind::TeamCompactNotify);
        let started: TeamCompactNotifyPayload =
            started.parse_payload().expect("team compact started");
        assert_eq!(started.status, TeamCompactStatus::Started);
        assert_eq!(started.agent_ids.len(), 2);

        let completed = loop {
            let envelope = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("team compaction should finish")
                .expect("team compact envelope");
            if envelope.kind != FrameKind::TeamCompactNotify {
                continue;
            }
            let payload: TeamCompactNotifyPayload =
                envelope.parse_payload().expect("team compact notify");
            if payload.status != TeamCompactStatus::Started {
                break payload;
            }
        };
        assert_eq!(completed.status, TeamCompactStatus::Completed);
        assert_eq!(completed.results.len(), 2);
        assert!(
            completed
                .results
                .iter()
                .all(|result| result.status == AgentCompactStatus::Completed)
        );
        assert!(
            completed
                .results
                .iter()
                .all(|result| old_agent_ids.contains(&result.old_agent_id))
        );

        let snapshot = team_snapshot(&fixture.host).await;
        for member_id in [&fixture.manager.id, &fixture.report.id] {
            let member = member_from_snapshot(snapshot.clone(), member_id);
            let session_id = member.session_id.expect("member session after compaction");
            let binding = snapshot
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .expect("member binding");
            let new_agent_id = binding
                .current_agent_id
                .as_ref()
                .expect("member remains live-bound");
            assert!(!old_agent_ids.contains(new_agent_id));
            let result = completed
                .results
                .iter()
                .find(|result| result.new_agent_id.as_ref() == Some(new_agent_id))
                .expect("completed result for new agent");
            assert_eq!(result.new_session_id.as_ref(), Some(&session_id));
        }
    }

    #[tokio::test]
    async fn team_first_message_records_report_session_id() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let outcome = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Please investigate the bug".to_owned(),
                None,
            )
            .await
            .expect("message report");

        assert_eq!(outcome.member_id, fixture.report.id);
        assert!(!outcome.queued);

        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        let session_id = report
            .session_id
            .clone()
            .expect("first spawn records session id");
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        let persisted_report = persisted
            .members
            .get(&fixture.report.id)
            .expect("persisted report");
        assert_eq!(persisted_report.session_id.as_ref(), Some(&session_id));

        let start = fixture
            .host
            .list_agents()
            .await
            .into_iter()
            .find(|start| start.agent_id == outcome.agent_id)
            .expect("spawned report agent");
        assert_eq!(start.origin, AgentOrigin::TeamMember);
        assert_eq!(start.team_id.as_ref(), Some(&fixture.team.id));
        assert_eq!(start.team_member_id.as_ref(), Some(&fixture.report.id));
    }

    #[tokio::test]
    async fn team_member_spawn_uses_union_of_project_roots() {
        let fixture = team_fixture().await;
        let second_root = fixture._dir.path().join("second-project-root");
        std::fs::create_dir_all(&second_root).expect("create second root");
        let second_root = second_root.to_string_lossy().to_string();
        fixture
            .host
            .create_project(ProjectCreatePayload {
                name: "Second Team Project".to_owned(),
                roots: vec![ProjectRootPath(second_root.clone())],
            })
            .await
            .expect("create second project");
        let second_project_id = {
            let state = fixture.host.state.lock().await;
            state
                .project_store
                .lock()
                .await
                .list()
                .expect("list projects")
                .into_iter()
                .find(|project| project.name == "Second Team Project")
                .expect("created second project")
                .id
        };
        fixture
            .host
            .update_team_member(TeamMemberUpdatePayload {
                id: fixture.report.id.clone(),
                name: fixture.report.name.clone(),
                description: fixture.report.description.clone(),
                profile: fixture.report.profile.clone(),
                project_ids: vec![fixture.project_id.clone(), second_project_id],
            })
            .await
            .expect("update report projects");
        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let outcome = fixture
            .host
            .message_team_member(
                manager_agent_id,
                report.id.clone(),
                "Use both projects".to_owned(),
                None,
            )
            .await
            .expect("message report");

        let start = fixture
            .host
            .list_agents()
            .await
            .into_iter()
            .find(|start| start.agent_id == outcome.agent_id)
            .expect("spawned report agent");
        assert_eq!(
            start.workspace_roots,
            vec![fixture.project_root.clone(), second_root]
        );
        assert_eq!(start.project_id.as_ref(), Some(&fixture.project_id));
    }

    #[tokio::test]
    async fn team_terminal_agent_unbinds_and_resumes_next_message() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let first = fixture
            .host
            .message_team_member(
                manager_agent_id.clone(),
                fixture.report.id.clone(),
                format!("First task {MOCK_DIE_AFTER_BUSY_SENTINEL}"),
                None,
            )
            .await
            .expect("first message spawns report");
        let first_session_id =
            member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id)
                .session_id
                .expect("first session id");

        wait_for_team_member_unbound(&fixture.host, &fixture.report.id).await;

        let second = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Follow-up after crash".to_owned(),
                None,
            )
            .await
            .expect("second message resumes after terminal unbind");
        assert_ne!(first.agent_id, second.agent_id);

        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        assert_eq!(report.session_id.as_ref(), Some(&first_session_id));
    }

    #[tokio::test]
    async fn team_subsequent_unbound_message_resumes_session() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;
        let first = fixture
            .host
            .message_team_member(
                manager_agent_id.clone(),
                fixture.report.id.clone(),
                "First task".to_owned(),
                None,
            )
            .await
            .expect("first message");
        let first_session_id =
            member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id)
                .session_id
                .expect("first session id");

        assert!(fixture.host.close_agent(&first.agent_id).await);

        let second = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Follow-up task".to_owned(),
                None,
            )
            .await
            .expect("second message resumes");
        assert_ne!(first.agent_id, second.agent_id);

        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        assert_eq!(report.session_id.as_ref(), Some(&first_session_id));
    }

    #[tokio::test]
    async fn team_message_member_rejects_report_caller() {
        let fixture = team_fixture().await;
        let report_agent_id = bind_team_member(&fixture.host, &fixture.report).await;

        let err = fixture
            .host
            .message_team_member(
                report_agent_id,
                fixture.report.id.clone(),
                "Try to delegate".to_owned(),
                None,
            )
            .await
            .expect_err("report caller should be rejected");
        assert!(err.starts_with("authorization:"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn team_resume_failure_marks_binding_failed() {
        let fixture = team_fixture().await;
        let report_agent_id = bind_team_member(&fixture.host, &fixture.report).await;
        let report = member_from_snapshot(team_snapshot(&fixture.host).await, &fixture.report.id);
        let bad_session_id = report.session_id.clone().expect("bound report session");
        {
            let state = fixture.host.state.lock().await;
            state
                .session_store
                .lock()
                .await
                .upsert_backend_session(
                    &BackendSession {
                        id: bad_session_id.clone(),
                        backend_kind: BackendKind::Claude,
                        workspace_roots: vec![fixture.project_root.clone()],
                        title: Some("Missing mock backend session".to_owned()),
                        token_count: None,
                        created_at_ms: Some(1),
                        updated_at_ms: Some(1),
                        resumable: true,
                    },
                    None,
                    Some(report.project_ids[0].clone()),
                    report.custom_agent_id.clone(),
                    None,
                )
                .expect("persist fake session");
        }
        {
            let registry = { fixture.host.state.lock().await.team_registry.clone() };
            let events = registry
                .clear_binding_by_agent(report_agent_id)
                .await
                .expect("clear report binding");
            fixture.host.fan_out_team_registry_events(events).await;
        }
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;

        let err = fixture
            .host
            .message_team_member(
                manager_agent_id,
                fixture.report.id.clone(),
                "Try resume".to_owned(),
                None,
            )
            .await
            .expect_err("unknown backend session should fail");
        assert!(
            err.contains("unknown mock session"),
            "unexpected error: {err}"
        );

        let snapshot = team_snapshot(&fixture.host).await;
        let binding = snapshot
            .bindings
            .iter()
            .find(|binding| binding.member_id == fixture.report.id)
            .expect("report binding");
        assert!(binding.current_agent_id.is_none());
        assert_eq!(binding.status, AgentControlStatus::Failed);
        let report = member_from_snapshot(snapshot, &fixture.report.id);
        assert!(report.session_id.is_none());
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        let persisted_report = persisted
            .members
            .get(&fixture.report.id)
            .expect("persisted report");
        assert!(persisted_report.session_id.is_none());
    }

    #[tokio::test]
    async fn team_delete_hard_removes_team_and_members() {
        let fixture = team_fixture().await;

        fixture
            .host
            .delete_team(TeamDeletePayload {
                id: fixture.team.id.clone(),
            })
            .await
            .expect("delete team");
        let snapshot = team_snapshot(&fixture.host).await;
        assert!(snapshot.teams.iter().all(|team| team.id != fixture.team.id));
        assert!(
            snapshot
                .members
                .iter()
                .all(|member| member.team_id != fixture.team.id)
        );
        assert!(snapshot.bindings.iter().all(|binding| {
            binding.member_id != fixture.manager.id && binding.member_id != fixture.report.id
        }));
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        assert!(!persisted.teams.contains_key(&fixture.team.id));
        assert!(!persisted.members.contains_key(&fixture.manager.id));
        assert!(!persisted.members.contains_key(&fixture.report.id));

        let err = fixture
            .host
            .rename_team(TeamRenamePayload {
                id: fixture.team.id.clone(),
                name: "Renamed Team".to_owned(),
            })
            .await
            .expect_err("deleted team rename should fail");
        assert_eq!(err.kind, crate::error::AppErrorKind::NotFound);
        assert!(err.message.contains("missing team"));
    }

    #[tokio::test]
    async fn concurrent_first_team_messages_spawn_at_most_one_agent() {
        let fixture = team_fixture().await;
        let manager_agent_id = bind_team_member(&fixture.host, &fixture.manager).await;
        let mut tasks = Vec::new();
        for index in 0..8 {
            let host = fixture.host.clone();
            let caller = manager_agent_id.clone();
            let member_id = fixture.report.id.clone();
            tasks.push(tokio::spawn(async move {
                host.message_team_member(
                    caller,
                    member_id,
                    format!("Concurrent task {index}"),
                    None,
                )
                .await
            }));
        }

        let mut success_count = 0;
        for task in tasks {
            match task.await.expect("message task should not panic") {
                Ok(_) => success_count += 1,
                Err(err) => assert!(
                    err.starts_with("conflict:"),
                    "unexpected concurrent message error: {err}"
                ),
            }
        }
        assert!(success_count >= 1);

        let report_agents = fixture
            .host
            .list_agents()
            .await
            .into_iter()
            .filter(|agent| agent.team_member_id.as_ref() == Some(&fixture.report.id))
            .collect::<Vec<_>>();
        assert_eq!(report_agents.len(), 1);
    }

    #[tokio::test]
    async fn team_delete_rejects_live_bound_member() {
        let fixture = team_fixture().await;
        bind_team_member(&fixture.host, &fixture.report).await;

        let err = fixture
            .host
            .delete_team_member(TeamMemberDeletePayload {
                id: fixture.report.id.clone(),
            })
            .await
            .expect_err("live-bound report should not delete");
        assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
        assert!(err.message.contains("live-bound"));
    }

    #[tokio::test]
    async fn team_references_block_custom_agent_but_project_delete_unassigns_members() {
        let fixture = team_fixture().await;

        let custom_agent_err = fixture
            .host
            .delete_custom_agent(CustomAgentDeletePayload {
                id: fixture.custom_agent_id.clone(),
            })
            .await
            .expect_err("custom agent reference should block delete");
        assert_eq!(custom_agent_err.kind, crate::error::AppErrorKind::Conflict);
        assert!(
            custom_agent_err
                .message
                .contains(r#"custom agent "Team Custom Agent""#)
        );
        assert!(custom_agent_err.message.contains(r#"team "Product Team""#));
        assert!(
            custom_agent_err
                .message
                .contains(r#"team member "Manager""#)
                || custom_agent_err.message.contains(r#"team member "Report""#)
        );
        assert!(!custom_agent_err.message.contains(&fixture.manager.id.0));
        assert!(!custom_agent_err.message.contains(&fixture.report.id.0));

        fixture
            .host
            .delete_project(ProjectDeletePayload {
                id: fixture.project_id.clone(),
            })
            .await
            .expect("project delete should detach team project refs");

        let snapshot = team_snapshot(&fixture.host).await;
        let manager = member_from_snapshot(snapshot.clone(), &fixture.manager.id);
        let report = member_from_snapshot(snapshot, &fixture.report.id);
        assert!(manager.project_ids.is_empty());
        assert!(report.project_ids.is_empty());

        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        assert_no_team_member_references_project(&persisted, &fixture.project_id);
        assert_agent_team_store_loads_with_current_refs(
            &fixture.host,
            &fixture.agent_team_store_path,
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_member_and_delete_custom_agent_serialize() {
        let race_test_guard = team_mutation_race_test_lock().lock().await;
        let fixture = team_race_fixture().await;
        assert!(fixture.temp_dir.path().exists());
        let hook = install_team_mutation_after_refs_test_hook(&fixture.host, "team_member_create");
        let create_host = fixture.host.clone();
        let create_payload = team_member_create_payload(
            &fixture,
            fixture.custom_agent_id.clone(),
            vec![fixture.project_id.clone()],
        );
        let create_task =
            tokio::spawn(async move { create_host.create_team_member(create_payload).await });

        hook.wait_until_reached().await;
        let delete_host = fixture.host.clone();
        let custom_agent_id = fixture.custom_agent_id.clone();
        let delete_task = tokio::spawn(async move {
            delete_host
                .delete_custom_agent(CustomAgentDeletePayload {
                    id: custom_agent_id,
                })
                .await
        });
        tokio::task::yield_now().await;
        hook.resume();

        let create_result = create_task.await.expect("create task should not panic");
        let delete_result = delete_task.await.expect("delete task should not panic");
        match (&create_result, &delete_result) {
            (Ok(()), Err(err)) => {
                assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
                assert!(err.message.contains(r#"custom agent "Race Custom Agent""#));
                assert!(err.message.contains(r#"team member "Race Report""#));
            }
            (Err(err), Ok(())) => {
                assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
                assert!(
                    err.message.contains("references missing custom agent"),
                    "unexpected create error: {}",
                    err.message
                );
            }
            (Ok(()), Ok(())) => panic!("create and delete both succeeded"),
            (Err(create_err), Err(delete_err)) => panic!(
                "create and delete both failed: create={}, delete={}",
                create_err, delete_err
            ),
        }
        assert_agent_team_store_loads_with_current_refs(
            &fixture.host,
            &fixture.agent_team_store_path,
        )
        .await;
        drop(hook);
        drop(race_test_guard);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_member_and_delete_project_serialize() {
        let race_test_guard = team_mutation_race_test_lock().lock().await;
        let fixture = team_race_fixture().await;
        assert!(fixture.temp_dir.path().exists());
        let hook = install_team_mutation_after_refs_test_hook(&fixture.host, "team_member_create");
        let create_host = fixture.host.clone();
        let create_payload = team_member_create_payload(
            &fixture,
            fixture.custom_agent_id.clone(),
            vec![fixture.project_id.clone()],
        );
        let create_task =
            tokio::spawn(async move { create_host.create_team_member(create_payload).await });

        hook.wait_until_reached().await;
        let delete_host = fixture.host.clone();
        let project_id = fixture.project_id.clone();
        let delete_task = tokio::spawn(async move {
            delete_host
                .delete_project(ProjectDeletePayload { id: project_id })
                .await
        });
        tokio::task::yield_now().await;
        hook.resume();

        let create_result = create_task.await.expect("create task should not panic");
        let delete_result = delete_task.await.expect("delete task should not panic");
        match (&create_result, &delete_result) {
            (Ok(()), Err(err)) => {
                panic!("delete should detach project refs instead of failing: {err}");
            }
            (Err(err), Ok(())) => {
                assert_eq!(err.kind, crate::error::AppErrorKind::Conflict);
                assert!(
                    err.message.contains("references missing project"),
                    "unexpected create error: {}",
                    err.message
                );
            }
            (Ok(()), Ok(())) => {
                let snapshot = team_snapshot(&fixture.host).await;
                let created = snapshot
                    .members
                    .iter()
                    .find(|member| member.name == "Race Report")
                    .expect("created race report");
                assert!(
                    !created.project_ids.contains(&fixture.project_id),
                    "created member retained deleted project ref"
                );
            }
            (Err(create_err), Err(delete_err)) => panic!(
                "create and delete both failed: create={}, delete={}",
                create_err, delete_err
            ),
        }
        let persisted = persisted_team_store(&fixture.agent_team_store_path);
        assert_no_team_member_references_project(&persisted, &fixture.project_id);
        assert_agent_team_store_loads_with_current_refs(
            &fixture.host,
            &fixture.agent_team_store_path,
        )
        .await;
        drop(hook);
        drop(race_test_guard);
    }

    #[tokio::test]
    async fn ai_reviewer_backend_resolution_uses_host_defaults() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");
        let host = spawn_host_with_store_paths_and_runtime_config(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
            HostRuntimeConfig::default(),
        )
        .expect("spawn host");

        let err = host
            .resolve_ai_reviewer_backend_kind(None)
            .await
            .expect_err("missing host backend should fail");
        assert!(
            err.contains("no default_backend or enabled backends"),
            "unexpected error: {err}"
        );

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::EnabledBackends {
                enabled_backends: vec![BackendKind::Codex, BackendKind::Claude],
            },
        })
        .await
        .expect("enable backends");
        assert_eq!(
            host.resolve_ai_reviewer_backend_kind(None)
                .await
                .expect("first enabled backend"),
            BackendKind::Claude
        );

        host.set_setting(SetSettingPayload {
            setting: HostSettingValue::DefaultBackend {
                default_backend: Some(BackendKind::Codex),
            },
        })
        .await
        .expect("set default backend");
        assert_eq!(
            host.resolve_ai_reviewer_backend_kind(None)
                .await
                .expect("default backend"),
            BackendKind::Codex
        );
        assert_eq!(
            host.resolve_ai_reviewer_backend_kind(Some(BackendKind::Antigravity))
                .await
                .expect("explicit backend override"),
            BackendKind::Antigravity
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ai_reviewer_non_claude_reaches_read_only_spawn_preconditions() {
        let dir = std::env::temp_dir().join(format!("tyde-host-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp host dir");
        let host = spawn_host_with_store_paths_and_runtime_config(
            dir.join("sessions.json"),
            dir.join("projects.json"),
            dir.join("settings.json"),
            HostRuntimeConfig::default(),
        )
        .expect("spawn host");
        host.state.lock().await.review_mcp.url.clear();

        let (review_tx, _review_rx) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        let project_store = {
            let state = host.state.lock().await;
            Arc::clone(&state.project_store)
        };
        let project = project_store
            .lock()
            .await
            .create(
                "Review Project".to_owned(),
                vec![ProjectRootPath("/tmp/review-root".to_owned())],
            )
            .expect("create review project");
        let review = Review {
            id: ReviewId("review-test".to_string()),
            project_id: project.id,
            origin_agent_id: AgentId("agent-test".to_string()),
            origin_session_id: SessionId("session-test".to_string()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![ProjectGitDiffPayload {
                root: ProjectRootPath("/tmp/review-root".to_string()),
                scope: ProjectDiffScope::Uncommitted,
                path: None,
                context_mode: DiffContextMode::Hunks,
                files: vec![ProjectGitDiffFile {
                    relative_path: "src/lib.rs".to_owned(),
                    is_binary: false,
                    hunks: Vec::new(),
                }],
            }],
            comments: Vec::new(),
            suggestions: Vec::new(),
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        };

        let (_reply, result) = host
            .spawn_ai_reviewer(crate::review::actor::ReviewAiSpawnRequest {
                review_id: review.id.clone(),
                review,
                backend_kind: Some(BackendKind::Codex),
                cost_hint: None,
                instructions: None,
                review_handle: ReviewHandle { tx: review_tx },
                reply,
            })
            .await;

        let err = result.expect_err("missing MCP should fail before spawning");
        assert!(
            err.contains("review feedback MCP server is unavailable"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supervisor_done_deadline_uses_live_delay_and_original_idle_since() {
        let idle_since = Instant::now();
        let agent_id = AgentId("supervisor-deadline".to_owned());
        let mut entries = HashMap::new();
        entries.insert(
            agent_id,
            SupervisorSchedulerEntry {
                last_activity_counter: 7,
                phase: SupervisorPhase::DoneAuthorized {
                    idle_since,
                    baseline: SupervisionBaseline {
                        last_user_message: "done".to_owned(),
                        kicks_since_user_message: 0,
                        session_id: None,
                    },
                    last_gate_evaluation_epoch: None,
                },
            },
        );
        let mut supervisor = protocol::SupervisorSettings::default();
        supervisor.enabled = true;
        supervisor.auto_compact_on_success = true;
        supervisor.auto_compact_inactivity_delay_seconds = 600;
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal {
                    settings: supervisor,
                    epoch: 1,
                },
                false,
            ),
            idle_since.checked_add(Duration::from_secs(600))
        );
        supervisor.auto_compact_inactivity_delay_seconds = 30;
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal {
                    settings: supervisor,
                    epoch: 2,
                },
                false,
            ),
            idle_since.checked_add(Duration::from_secs(30))
        );
    }

    #[test]
    fn supervisor_failed_gate_is_suppressed_for_only_its_settings_epoch() {
        let agent_id = AgentId("supervisor-gate".to_owned());
        let idle_since = Instant::now();
        let mut entries = HashMap::new();
        entries.insert(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: 9,
                phase: SupervisorPhase::DoneAuthorized {
                    idle_since,
                    baseline: SupervisionBaseline {
                        last_user_message: "done".to_owned(),
                        kicks_since_user_message: 0,
                        session_id: None,
                    },
                    last_gate_evaluation_epoch: None,
                },
            },
        );
        mark_supervisor_gate_evaluated(&mut entries, &agent_id, 4);
        let mut supervisor = protocol::SupervisorSettings::default();
        supervisor.enabled = true;
        supervisor.auto_compact_on_success = true;
        supervisor.auto_compact_inactivity_delay_seconds = 1;
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal {
                    settings: supervisor,
                    epoch: 4,
                },
                false,
            ),
            None
        );
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal {
                    settings: supervisor,
                    epoch: 5,
                },
                false,
            ),
            idle_since.checked_add(Duration::from_secs(1))
        );
    }

    #[test]
    fn supervisor_fresh_idle_observation_starts_a_new_interval() {
        let now = Instant::now();
        let idle = crate::agent::registry::AgentStatus {
            started: true,
            turn_completed: true,
            activity_counter: 11,
            ..Default::default()
        };
        assert!(matches!(
            supervisor_phase_for_fresh_observation(&idle, now),
            SupervisorPhase::Debouncing { idle_since } if idle_since == now
        ));

        let active = crate::agent::registry::AgentStatus {
            is_thinking: true,
            ..idle
        };
        assert!(matches!(
            supervisor_phase_for_fresh_observation(&active, now),
            SupervisorPhase::Active
        ));
    }

    #[test]
    fn supervisor_kick_is_active_before_backend_typing_starts() {
        let now = Instant::now();
        let mut status = crate::agent::registry::AgentStatus {
            started: true,
            turn_completed: true,
            activity_counter: 21,
            ..Default::default()
        };

        mark_supervisor_kick_pending(&mut status);
        status.activity_counter += 1;

        assert!(status.is_active());
        assert!(matches!(
            supervisor_phase_for_fresh_observation(&status, now),
            SupervisorPhase::Active
        ));
    }

    #[tokio::test]
    async fn settings_commit_during_verdict_await_rearms_without_kick() {
        let fixture = compact_fixture().await;
        let (agent_id, session_id) =
            spawn_idle_user_agent(&fixture.host, "stale verdict settings race").await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let context = observation
            .handle
            .read_supervision_context()
            .await
            .expect("supervision context");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let cached_settings = fixture.host.supervisor_settings_signal().await;
        let idle_since = Instant::now();
        let baseline = SupervisionBaseline {
            last_user_message: context.last_user_message.expect("last user message"),
            kicks_since_user_message: context.kicks_since_user_message,
            session_id: Some(session_id),
        };
        let activity_counter = observation.status.activity_counter;
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::VerdictInFlight {
                    idle_since,
                    baseline: baseline.clone(),
                    attempts_started: 1,
                    verdict_settings: VerdictSettingsFingerprint::from(cached_settings.settings),
                },
            },
        )]);

        let (entered, release) = install_supervisor_verdict_post_sample_test_gate(agent_id.clone());
        let host = fixture.host.clone();
        let result_agent_id = agent_id.clone();
        let result_baseline = baseline.clone();
        let acceptance = tokio::spawn(async move {
            accept_supervision_verdict_result(
                &host,
                &mut entries,
                cached_settings,
                SupervisorVerdictTaskResult {
                    agent_id: result_agent_id,
                    activity_counter,
                    baseline: result_baseline,
                    attempts_started: 1,
                    verdict_settings: VerdictSettingsFingerprint::from(cached_settings.settings),
                    result: Ok(crate::agent::supervisor::SupervisionVerdict::Continue {
                        message: "stale kick must not be sent".to_owned(),
                    }),
                },
                Instant::now(),
            )
            .await;
            entries
        });
        entered
            .await
            .expect("verdict handler sampled the original settings epoch");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 2 },
            })
            .await
            .expect("commit settings during post-sample await window");
        assert_ne!(
            fixture.host.supervisor_settings_signal().await.epoch,
            cached_settings.epoch
        );
        release.send(()).expect("release verdict handler");
        let mut entries = acceptance.await.expect("verdict acceptance task");

        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::RetryPending {
                idle_since: preserved,
                attempts_started: 1,
                last_failure_kind: SupervisionRetryReason::SettingsChanged,
                ..
            }) if *preserved == idle_since
        ));
        let live_settings = fixture.host.supervisor_settings_signal().await;
        let context_after_stale_result = observation
            .handle
            .read_supervision_context()
            .await
            .expect("supervision context after stale result");
        assert_eq!(
            context_after_stale_result.kicks_since_user_message,
            baseline.kicks_since_user_message
        );
        apply_live_retry_settings(
            &agent_id,
            entries.get_mut(&agent_id).expect("scheduler entry"),
            live_settings.settings,
        );
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::RetryPending { .. })
        ));
        entries.get_mut(&agent_id).expect("scheduler entry").phase =
            SupervisorPhase::VerdictInFlight {
                idle_since,
                baseline: baseline.clone(),
                attempts_started: 2,
                verdict_settings: VerdictSettingsFingerprint::from(live_settings.settings),
            };
        accept_supervision_verdict_result(
            &fixture.host,
            &mut entries,
            live_settings,
            SupervisorVerdictTaskResult {
                agent_id: agent_id.clone(),
                activity_counter,
                baseline: baseline.clone(),
                attempts_started: 2,
                verdict_settings: VerdictSettingsFingerprint::from(live_settings.settings),
                result: Ok(crate::agent::supervisor::SupervisionVerdict::Done),
            },
            Instant::now(),
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::DoneAuthorized {
                idle_since: preserved,
                ..
            }) if *preserved == idle_since
        ));

        accept_supervision_verdict_result(
            &fixture.host,
            &mut entries,
            live_settings,
            SupervisorVerdictTaskResult {
                agent_id: agent_id.clone(),
                activity_counter,
                baseline,
                attempts_started: 2,
                verdict_settings: VerdictSettingsFingerprint::from(live_settings.settings),
                result: Ok(crate::agent::supervisor::SupervisionVerdict::AwaitingUser),
            },
            Instant::now(),
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::DoneAuthorized { .. })
        ));
    }

    #[tokio::test]
    async fn deadline_launch_reads_live_epoch_without_rearm_churn() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "live deadline epoch").await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let settings_rx = fixture.host.supervisor_settings_receiver().await;
        let stale_settings = *settings_rx.borrow();
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 2 },
            })
            .await
            .expect("commit settings before deadline decision");
        let live_settings = fixture.host.supervisor_settings_signal().await;
        assert_ne!(live_settings.epoch, stale_settings.epoch);

        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let idle_since = Instant::now()
            .checked_sub(SUPERVISION_DEBOUNCE)
            .expect("past-due debounce instant");
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: observation.status.activity_counter,
                phase: SupervisorPhase::Debouncing { idle_since },
            },
        )]);
        let (verdict_tx, _verdict_rx) = mpsc::unbounded_channel();
        let (compaction_tx, _compaction_rx) = mpsc::unbounded_channel();
        let semaphore = Arc::new(Semaphore::new(1));
        let mut verdict_task_state = SupervisorVerdictTaskState::default();

        launch_supervision_verdict(
            &fixture.host,
            &mut entries,
            agent_id.clone(),
            stale_settings,
            &verdict_tx,
            &semaphore,
            &mut verdict_task_state,
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Debouncing { idle_since: preserved })
                if *preserved == idle_since
        ));

        process_supervisor_deadlines_from_signal(
            &fixture.host,
            &mut entries,
            &settings_rx,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
        )
        .await;

        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::VerdictInFlight {
                verdict_settings,
                idle_since: preserved,
                ..
            }) if *verdict_settings == VerdictSettingsFingerprint::from(live_settings.settings)
                && *preserved == idle_since
        ));

        apply_supervisor_settings_change(
            &fixture.host,
            &mut entries,
            stale_settings,
            live_settings,
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::VerdictInFlight {
                verdict_settings,
                ..
            }) if *verdict_settings == VerdictSettingsFingerprint::from(live_settings.settings)
        ));
    }

    #[tokio::test]
    async fn disable_then_enable_observes_idle_agent_with_fresh_interval() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "restart interval").await;
        let old_idle_since = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("old instant");
        let baseline = SupervisionBaseline {
            last_user_message: "restart interval".to_owned(),
            kicks_since_user_message: 0,
            session_id: None,
        };
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: 1,
                phase: SupervisorPhase::RetryPending {
                    idle_since: old_idle_since,
                    baseline,
                    attempts_started: 1,
                    due_at: Instant::now().checked_add(Duration::from_secs(30)).unwrap(),
                    last_failure_kind: SupervisionRetryReason::Failure(
                        crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    ),
                    verdict_settings: VerdictSettingsFingerprint::from(
                        protocol::SupervisorSettings::default(),
                    ),
                },
            },
        )]);
        let mut enabled = protocol::SupervisorSettings::default();
        enabled.enabled = true;
        let mut disabled = enabled;
        disabled.enabled = false;

        apply_supervisor_settings_change(
            &fixture.host,
            &mut entries,
            SupervisorSettingsSignal {
                settings: enabled,
                epoch: 1,
            },
            SupervisorSettingsSignal {
                settings: disabled,
                epoch: 2,
            },
        )
        .await;
        assert!(entries.is_empty());

        let reenabled_at = Instant::now();
        apply_supervisor_settings_change(
            &fixture.host,
            &mut entries,
            SupervisorSettingsSignal {
                settings: disabled,
                epoch: 2,
            },
            SupervisorSettingsSignal {
                settings: enabled,
                epoch: 3,
            },
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Debouncing { idle_since })
                if *idle_since >= reenabled_at && *idle_since != old_idle_since
        ));
    }

    #[tokio::test]
    async fn activity_cancels_pending_retry_and_resets_generation() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "retry activity reset").await;
        let status = fixture
            .host
            .agent_status_snapshot(&agent_id)
            .await
            .expect("agent status");
        let idle_since = Instant::now();
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: status.activity_counter,
                phase: SupervisorPhase::RetryPending {
                    idle_since,
                    baseline: SupervisionBaseline {
                        last_user_message: "retry activity reset".to_owned(),
                        kicks_since_user_message: 0,
                        session_id: None,
                    },
                    attempts_started: 1,
                    due_at: idle_since.checked_add(Duration::from_secs(30)).unwrap(),
                    last_failure_kind: SupervisionRetryReason::Failure(
                        crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    ),
                    verdict_settings: VerdictSettingsFingerprint::from(
                        protocol::SupervisorSettings::default(),
                    ),
                },
            },
        )]);
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let status_handle = fixture
            .host
            .agent_status_handle(&agent_id)
            .await
            .expect("agent status handle");
        status_handle
            .update(|status| {
                status.activity_counter = status.activity_counter.saturating_add(1);
                status.is_thinking = true;
                status.turn_completed = false;
            })
            .await;
        observe_supervised_agents(&fixture.host, &mut entries).await;

        let entry = entries.get(&agent_id).expect("scheduler entry");
        assert_ne!(entry.last_activity_counter, status.activity_counter);
        assert!(matches!(entry.phase, SupervisorPhase::Active));
    }

    #[tokio::test]
    async fn failed_verdict_retries_then_continue_delivers_one_kick() {
        let fixture = compact_fixture().await;
        let (agent_id, session_id) =
            spawn_idle_user_agent(&fixture.host, "retry then recover").await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let settings = fixture.host.supervisor_settings_signal().await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let context = observation
            .handle
            .read_supervision_context()
            .await
            .expect("supervision context");
        let idle_since = Instant::now();
        let baseline = SupervisionBaseline {
            last_user_message: context.last_user_message.expect("last user message"),
            kicks_since_user_message: context.kicks_since_user_message,
            session_id: Some(session_id),
        };
        let activity_counter = observation.status.activity_counter;
        let fingerprint = VerdictSettingsFingerprint::from(settings.settings);
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::VerdictInFlight {
                    idle_since,
                    baseline: baseline.clone(),
                    attempts_started: 1,
                    verdict_settings: fingerprint,
                },
            },
        )]);

        accept_supervision_verdict_result(
            &fixture.host,
            &mut entries,
            settings,
            SupervisorVerdictTaskResult {
                agent_id: agent_id.clone(),
                activity_counter,
                baseline: baseline.clone(),
                attempts_started: 1,
                verdict_settings: fingerprint,
                result: Err(crate::agent::supervisor::SupervisionFailure {
                    kind: crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    message: "temporary outage".to_owned(),
                }),
            },
            Instant::now(),
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::RetryPending {
                attempts_started: 1,
                ..
            })
        ));

        entries.get_mut(&agent_id).expect("scheduler entry").phase =
            SupervisorPhase::VerdictInFlight {
                idle_since,
                baseline: baseline.clone(),
                attempts_started: 2,
                verdict_settings: fingerprint,
            };
        accept_supervision_verdict_result(
            &fixture.host,
            &mut entries,
            settings,
            SupervisorVerdictTaskResult {
                agent_id: agent_id.clone(),
                activity_counter,
                baseline,
                attempts_started: 2,
                verdict_settings: fingerprint,
                result: Ok(crate::agent::supervisor::SupervisionVerdict::Continue {
                    message: "continue after recovery".to_owned(),
                }),
            },
            Instant::now(),
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Active)
        ));
        let mut kicks = 0;
        for _ in 0..100 {
            kicks = observation
                .handle
                .read_supervision_context()
                .await
                .expect("supervision context after recovery")
                .kicks_since_user_message;
            if kicks == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(kicks, 1);
    }

    #[tokio::test]
    async fn failure_exhaustion_appends_once_and_only_then_becomes_dormant() {
        let fixture = compact_fixture().await;
        let (agent_id, session_id) =
            spawn_idle_user_agent(&fixture.host, "terminal supervisor failure").await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 0 },
            })
            .await
            .expect("set immediate exhaustion cap");
        let settings = fixture.host.supervisor_settings_signal().await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let context = observation
            .handle
            .read_supervision_context()
            .await
            .expect("supervision context");
        let baseline = SupervisionBaseline {
            last_user_message: context.last_user_message.expect("last user message"),
            kicks_since_user_message: context.kicks_since_user_message,
            session_id: Some(session_id),
        };
        let activity_counter = observation.status.activity_counter;
        let idle_since = Instant::now();
        let fingerprint = VerdictSettingsFingerprint::from(settings.settings);
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::VerdictInFlight {
                    idle_since,
                    baseline: baseline.clone(),
                    attempts_started: 1,
                    verdict_settings: fingerprint,
                },
            },
        )]);

        accept_supervision_verdict_result(
            &fixture.host,
            &mut entries,
            settings,
            SupervisorVerdictTaskResult {
                agent_id: agent_id.clone(),
                activity_counter,
                baseline,
                attempts_started: 1,
                verdict_settings: fingerprint,
                result: Err(crate::agent::supervisor::SupervisionFailure {
                    kind: crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    message: "private backend detail".to_owned(),
                }),
            },
            Instant::now(),
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Dormant { .. })
        ));

        exhaust_supervision_by_failure(
            &fixture.host,
            &mut entries,
            &agent_id,
            activity_counter,
            idle_since,
            1,
            settings,
        )
        .await;
        let history = observation
            .handle
            .fetch_session_history(None, 100)
            .await
            .expect("actor history");
        let warnings = history
            .events
            .iter()
            .filter_map(|event| match event {
                ChatEvent::MessageAdded(message)
                    if matches!(message.sender, MessageSender::Warning)
                        && message.content.starts_with("Supervisor could not verify") =>
                {
                    Some(message.content.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            "Supervisor could not verify whether this task was complete after 1 attempt and has stopped retrying. Send a follow-up message if you want the agent to continue."
        );
        assert!(!warnings[0].contains("private backend detail"));
        assert!(!warnings[0].contains("BackendStream"));
    }

    #[tokio::test]
    async fn settings_change_at_warning_gate_rejects_stale_append_and_preserves_backoff() {
        let fixture = compact_fixture().await;
        let (agent_id, session_id) =
            spawn_idle_user_agent(&fixture.host, "warning settings race").await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 0 },
            })
            .await
            .expect("set initial retry cap");
        let expected_settings = fixture.host.supervisor_settings_signal().await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let context = observation
            .handle
            .read_supervision_context()
            .await
            .expect("supervision context");
        let baseline = SupervisionBaseline {
            last_user_message: context.last_user_message.expect("last user message"),
            kicks_since_user_message: context.kicks_since_user_message,
            session_id: Some(session_id),
        };
        let activity_counter = observation.status.activity_counter;
        let idle_since = Instant::now();
        let failed_at = Instant::now();
        let fingerprint = VerdictSettingsFingerprint::from(expected_settings.settings);
        let entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::VerdictInFlight {
                    idle_since,
                    baseline: baseline.clone(),
                    attempts_started: 1,
                    verdict_settings: fingerprint,
                },
            },
        )]);
        let (entered, release) =
            crate::agent::install_append_supervisor_warning_test_gate(agent_id.clone());
        let host = fixture.host.clone();
        let result_agent_id = agent_id.clone();
        let acceptance = tokio::spawn(async move {
            let mut entries = entries;
            accept_supervision_verdict_result(
                &host,
                &mut entries,
                expected_settings,
                SupervisorVerdictTaskResult {
                    agent_id: result_agent_id,
                    activity_counter,
                    baseline,
                    attempts_started: 1,
                    verdict_settings: fingerprint,
                    result: Err(crate::agent::supervisor::SupervisionFailure {
                        kind: crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                        message: "settings race failure".to_owned(),
                    }),
                },
                failed_at,
            )
            .await;
            entries
        });

        entered
            .await
            .expect("warning command reached final settings gate");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 1 },
            })
            .await
            .expect("raise retry cap at warning gate");
        let raised_settings = fixture.host.supervisor_settings_signal().await;
        release.send(()).expect("release warning settings gate");
        let entries = acceptance.await.expect("verdict acceptance task");
        let expected_due = failed_at
            .checked_add(SUPERVISION_RETRY_DELAYS[0])
            .expect("first retry due");
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::RetryPending {
                attempts_started: 1,
                due_at,
                verdict_settings,
                ..
            }) if *due_at == expected_due
                && *verdict_settings == VerdictSettingsFingerprint::from(raised_settings.settings)
        ));
        let history = observation
            .handle
            .fetch_session_history(None, 100)
            .await
            .expect("actor history after settings race");
        assert!(!history.events.iter().any(|event| matches!(
            event,
            ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::Warning)
                    && message.content.starts_with("Supervisor could not verify")
        )));
    }

    #[tokio::test]
    async fn failure_backed_live_cap_reduction_warns_but_settings_only_does_not() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "live retry cap reduction").await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 5 },
            })
            .await
            .expect("set raised retry cap");
        let previous = fixture.host.supervisor_settings_signal().await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let activity_counter = observation.status.activity_counter;
        let idle_since = Instant::now();
        let baseline = SupervisionBaseline {
            last_user_message: "live retry cap reduction".to_owned(),
            kicks_since_user_message: 0,
            session_id: observation.start.session_id.clone(),
        };
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::RetryPending {
                    idle_since,
                    baseline: baseline.clone(),
                    attempts_started: 2,
                    due_at: idle_since.checked_add(Duration::from_secs(60)).unwrap(),
                    last_failure_kind: SupervisionRetryReason::Failure(
                        crate::agent::supervisor::SupervisionFailureKind::Timeout,
                    ),
                    verdict_settings: VerdictSettingsFingerprint::from(previous.settings),
                },
            },
        )]);
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 1 },
            })
            .await
            .expect("lower retry cap");
        let current = fixture.host.supervisor_settings_signal().await;
        apply_supervisor_settings_change(&fixture.host, &mut entries, previous, current).await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Dormant { .. })
        ));
        let history = observation
            .handle
            .fetch_session_history(None, 100)
            .await
            .expect("actor history");
        assert!(history.events.iter().any(|event| matches!(
            event,
            ChatEvent::MessageAdded(message)
                if matches!(message.sender, MessageSender::Warning)
                    && message.content.contains("after 2 attempts")
        )));

        let settings_only_id = AgentId("settings-only-exhaustion".to_owned());
        entries.insert(
            settings_only_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: activity_counter,
                phase: SupervisorPhase::RetryPending {
                    idle_since,
                    baseline,
                    attempts_started: 2,
                    due_at: idle_since,
                    last_failure_kind: SupervisionRetryReason::SettingsChanged,
                    verdict_settings: VerdictSettingsFingerprint::from(previous.settings),
                },
            },
        );
        assert!(matches!(
            apply_live_retry_settings(
                &settings_only_id,
                entries
                    .get_mut(&settings_only_id)
                    .expect("settings-only entry"),
                current.settings,
            ),
            LiveRetrySettingsResult::SettingsExhausted {
                attempts_started: 2,
                ..
            }
        ));
    }

    async fn accept_next_supervisor_task_event(
        host: &HostHandle,
        entries: &mut HashMap<AgentId, SupervisorSchedulerEntry>,
        settings: SupervisorSettingsSignal,
        verdict_task_state: &mut SupervisorVerdictTaskState,
        verdict_rx: &mut mpsc::UnboundedReceiver<SupervisorVerdictTaskEvent>,
        result_dequeued_at: Instant,
    ) {
        let event = verdict_rx.recv().await.expect("supervisor task result");
        assert!(
            verdict_task_state.finish(event.task_id),
            "result must clear the actual scheduler-owned task"
        );
        accept_supervision_verdict_result(
            host,
            entries,
            settings,
            event.result,
            result_dequeued_at,
        )
        .await;
    }

    #[tokio::test]
    async fn aborted_verdict_task_releases_permit_and_reports_completion() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&semaphore)
            .try_acquire_owned()
            .expect("test permit");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut task_state = SupervisorVerdictTaskState::default();
        let task_id = task_state.reserve().expect("reserve task");
        let completion = SupervisorVerdictTaskCompletion {
            task_id,
            tx,
            permit: Some(permit),
            aborted: Some(SupervisorVerdictTaskResult {
                agent_id: AgentId("aborted-verdict".to_owned()),
                activity_counter: 1,
                baseline: SupervisionBaseline {
                    last_user_message: "request".to_owned(),
                    kicks_since_user_message: 0,
                    session_id: None,
                },
                attempts_started: 1,
                verdict_settings: VerdictSettingsFingerprint::from(
                    protocol::SupervisorSettings::default(),
                ),
                result: Err(crate::agent::supervisor::SupervisionFailure {
                    kind: crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    message: "task aborted".to_owned(),
                }),
            }),
        };
        let task = tokio::spawn(async move {
            let _completion = completion;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        let event = rx.recv().await.expect("abort completion event");
        assert_eq!(semaphore.available_permits(), 1);
        assert!(task_state.finish(event.task_id));
        assert!(!task_state.is_active());
        assert!(matches!(
            event.result.result,
            Err(crate::agent::supervisor::SupervisionFailure {
                kind: crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn production_retry_scheduler_starts_exact_bounded_calls_at_due_deadlines() {
        for (retry_attempts, expected_calls) in [(0, 1_u8), (1, 2), (5, 6)] {
            let fixture = compact_fixture_without_supervisor_worker().await;
            fixture
                .host
                .set_setting(SetSettingPayload {
                    setting: HostSettingValue::SupervisorRetryAttempts {
                        count: retry_attempts,
                    },
                })
                .await
                .expect("set retry budget");
            let (agent_id, _) = spawn_idle_user_agent(
                &fixture.host,
                &format!(
                    "{} {}",
                    crate::backend::mock::MOCK_USER_BUBBLES_SENTINEL,
                    crate::agent::supervisor::MOCK_SUPERVISOR_ERROR,
                ),
            )
            .await;
            fixture
                .host
                .set_setting(SetSettingPayload {
                    setting: HostSettingValue::SupervisorEnabled { enabled: true },
                })
                .await
                .expect("enable supervisor");
            let settings = fixture.host.supervisor_settings_signal().await;
            let observation = fixture
                .host
                .activity_summary_observation(&agent_id)
                .await
                .expect("idle observation");
            let launch_at = Instant::now();
            let mut entries = HashMap::from([(
                agent_id.clone(),
                SupervisorSchedulerEntry {
                    last_activity_counter: observation.status.activity_counter,
                    phase: SupervisorPhase::Debouncing {
                        idle_since: launch_at.checked_sub(SUPERVISION_DEBOUNCE).unwrap(),
                    },
                },
            )]);
            let (verdict_tx, mut verdict_rx) = mpsc::unbounded_channel();
            let (compaction_tx, _compaction_rx) = mpsc::unbounded_channel();
            let semaphore = Arc::new(Semaphore::new(1));
            let mut verdict_task_state = SupervisorVerdictTaskState::default();
            let (mut starts, releases) =
                install_supervisor_verdict_call_test_gate(agent_id.clone());

            for expected_attempt in 1..=expected_calls {
                let due_at = match entries.get(&agent_id).map(|entry| &entry.phase) {
                    Some(SupervisorPhase::Debouncing { .. }) => launch_at,
                    Some(SupervisorPhase::RetryPending { due_at, .. }) => *due_at,
                    _ => panic!("attempt {expected_attempt} must have a due phase"),
                };
                if expected_attempt > 1 {
                    process_supervisor_deadlines_at(
                        &fixture.host,
                        &mut entries,
                        settings,
                        &verdict_tx,
                        &compaction_tx,
                        &semaphore,
                        &mut verdict_task_state,
                        due_at.checked_sub(Duration::from_nanos(1)).unwrap(),
                    )
                    .await;
                    assert!(!verdict_task_state.is_active());
                    assert!(starts.try_recv().is_err(), "retry started before due");
                }
                process_supervisor_deadlines_at(
                    &fixture.host,
                    &mut entries,
                    settings,
                    &verdict_tx,
                    &compaction_tx,
                    &semaphore,
                    &mut verdict_task_state,
                    due_at,
                )
                .await;
                let start = starts.recv().await.expect("recorded verdict call start");
                assert_eq!(start.agent_id, agent_id);
                assert_eq!(start.activity_counter, observation.status.activity_counter);
                assert_eq!(start.attempts_started, expected_attempt);
                assert_eq!(start.cost_hint, Some(protocol::SpawnCostHint::Low));
                assert!(verdict_task_state.is_active());
                releases.send(()).expect("release verdict call");
                let result_at = due_at.checked_add(Duration::from_secs(1)).unwrap();
                accept_next_supervisor_task_event(
                    &fixture.host,
                    &mut entries,
                    settings,
                    &mut verdict_task_state,
                    &mut verdict_rx,
                    result_at,
                )
                .await;
                if expected_attempt < expected_calls {
                    let expected_due = result_at
                        .checked_add(SUPERVISION_RETRY_DELAYS[usize::from(expected_attempt - 1)])
                        .unwrap();
                    assert!(matches!(
                        entries.get(&agent_id).map(|entry| &entry.phase),
                        Some(SupervisorPhase::RetryPending { due_at, .. })
                            if *due_at == expected_due
                    ));
                }
            }

            assert!(matches!(
                entries.get(&agent_id).map(|entry| &entry.phase),
                Some(SupervisorPhase::Dormant { .. })
            ));
            assert_eq!(
                supervisor_next_deadline(&entries, settings, verdict_task_state.is_active()),
                None
            );
            assert!(starts.try_recv().is_err());
            let history = observation
                .handle
                .fetch_session_history(None, 100)
                .await
                .expect("supervisor exhaustion history");
            let expected_attempt_label = if expected_calls == 1 {
                "attempt"
            } else {
                "attempts"
            };
            let expected_copy = format!(
                "Supervisor could not verify whether this task was complete after {expected_calls} {expected_attempt_label} and has stopped retrying. Send a follow-up message if you want the agent to continue."
            );
            assert_eq!(
                history
                    .events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        ChatEvent::MessageAdded(message)
                            if matches!(message.sender, MessageSender::Warning)
                                && message.content == expected_copy
                    ))
                    .count(),
                1
            );
            remove_supervisor_verdict_call_test_gate(&agent_id);
        }
    }

    #[tokio::test]
    async fn production_scheduler_occupancy_survives_activity_and_disable_phase_resets() {
        let fixture = compact_fixture_without_supervisor_worker().await;
        let (agent_id, _) = spawn_idle_user_agent(
            &fixture.host,
            &format!(
                "occupancy {}",
                crate::backend::mock::MOCK_USER_BUBBLES_SENTINEL,
            ),
        )
        .await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let settings = fixture.host.supervisor_settings_signal().await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("idle observation");
        let status_handle = fixture
            .host
            .agent_status_handle(&agent_id)
            .await
            .expect("agent status handle");
        let now = Instant::now();
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: observation.status.activity_counter,
                phase: SupervisorPhase::Debouncing {
                    idle_since: now.checked_sub(SUPERVISION_DEBOUNCE).unwrap(),
                },
            },
        )]);
        let (verdict_tx, mut verdict_rx) = mpsc::unbounded_channel();
        let (compaction_tx, _compaction_rx) = mpsc::unbounded_channel();
        let semaphore = Arc::new(Semaphore::new(1));
        let mut verdict_task_state = SupervisorVerdictTaskState::default();
        let (mut starts, releases) = install_supervisor_verdict_call_test_gate(agent_id.clone());

        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            settings,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            now,
        )
        .await;
        let first_start = starts.recv().await.expect("first held verdict call");
        assert_eq!(first_start.attempts_started, 1);

        assert!(
            observation
                .handle
                .send_input(AgentInput::SendMessage(SendMessagePayload {
                    message: crate::backend::mock::MOCK_SLOW_TURN_SENTINEL.to_owned(),
                    images: None,
                    origin: Some(MessageOrigin::User),
                    tool_response: None,
                }))
                .await
        );
        wait_for_agent_active(&fixture.host, &agent_id).await;
        observe_supervised_agents(&fixture.host, &mut entries).await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Active)
        ));
        assert!(verdict_task_state.is_active());
        assert!(starts.try_recv().is_err());
        status_handle
            .update(|status| {
                status.activity_counter = status.activity_counter.saturating_add(1);
                status.is_thinking = false;
                status.turn_completed = true;
            })
            .await;
        observe_supervised_agents(&fixture.host, &mut entries).await;
        let fresh_due = match entries.get(&agent_id).map(|entry| &entry.phase) {
            Some(SupervisorPhase::Debouncing { idle_since }) => {
                idle_since.checked_add(SUPERVISION_DEBOUNCE).unwrap()
            }
            _ => panic!("new activity generation must debounce"),
        };
        for _ in 0..100 {
            assert_eq!(
                supervisor_next_deadline(&entries, settings, verdict_task_state.is_active()),
                None
            );
            process_supervisor_deadlines_at(
                &fixture.host,
                &mut entries,
                settings,
                &verdict_tx,
                &compaction_tx,
                &semaphore,
                &mut verdict_task_state,
                fresh_due,
            )
            .await;
        }
        assert!(starts.try_recv().is_err());

        releases.send(()).expect("release stale activity call");
        accept_next_supervisor_task_event(
            &fixture.host,
            &mut entries,
            settings,
            &mut verdict_task_state,
            &mut verdict_rx,
            Instant::now(),
        )
        .await;
        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            settings,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            fresh_due,
        )
        .await;
        let second_start = starts.recv().await.expect("fresh activity call");
        assert_ne!(second_start.activity_counter, first_start.activity_counter);

        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: false },
            })
            .await
            .expect("disable supervisor while call is held");
        let disabled = fixture.host.supervisor_settings_signal().await;
        apply_supervisor_settings_change(&fixture.host, &mut entries, settings, disabled).await;
        assert!(entries.is_empty());
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("re-enable supervisor while old call is held");
        let reenabled = fixture.host.supervisor_settings_signal().await;
        apply_supervisor_settings_change(&fixture.host, &mut entries, disabled, reenabled).await;
        let reenabled_due = match entries.get(&agent_id).map(|entry| &entry.phase) {
            Some(SupervisorPhase::Debouncing { idle_since }) => {
                idle_since.checked_add(SUPERVISION_DEBOUNCE).unwrap()
            }
            _ => panic!("re-enabled idle generation must debounce"),
        };
        assert_eq!(
            supervisor_next_deadline(&entries, reenabled, verdict_task_state.is_active()),
            None
        );
        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            reenabled,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            reenabled_due,
        )
        .await;
        assert!(starts.try_recv().is_err());

        releases.send(()).expect("release pre-disable call");
        accept_next_supervisor_task_event(
            &fixture.host,
            &mut entries,
            reenabled,
            &mut verdict_task_state,
            &mut verdict_rx,
            Instant::now(),
        )
        .await;
        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            reenabled,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            reenabled_due,
        )
        .await;
        let third_start = starts.recv().await.expect("post-enable call");
        assert_eq!(third_start.activity_counter, second_start.activity_counter);
        releases.send(()).expect("release post-enable call");
        accept_next_supervisor_task_event(
            &fixture.host,
            &mut entries,
            reenabled,
            &mut verdict_task_state,
            &mut verdict_rx,
            Instant::now(),
        )
        .await;
        remove_supervisor_verdict_call_test_gate(&agent_id);
    }

    #[tokio::test]
    async fn actor_verdict_start_rejects_activity_ordered_before_authorization() {
        let fixture = compact_fixture_without_supervisor_worker().await;
        let (agent_id, _) = spawn_idle_user_agent(
            &fixture.host,
            &format!(
                "actor ordering {}",
                crate::backend::mock::MOCK_USER_BUBBLES_SENTINEL,
            ),
        )
        .await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let settings = fixture.host.supervisor_settings_signal().await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("idle observation");

        assert!(
            observation
                .handle
                .send_input(AgentInput::SendMessage(SendMessagePayload {
                    message: crate::backend::mock::MOCK_SLOW_TURN_SENTINEL.to_owned(),
                    images: None,
                    origin: Some(MessageOrigin::User),
                    tool_response: None,
                }))
                .await
        );
        let result = observation
            .handle
            .begin_supervisor_verdict_if_inactive(
                observation.status.activity_counter,
                VerdictSettingsFingerprint::from(settings.settings),
                fixture.host.supervisor_settings_receiver().await,
            )
            .await;

        assert!(matches!(
            result,
            SupervisorVerdictStart::Rejected {
                reason: crate::agent::SupervisorVerdictStartRejection::ActivityChanged,
                live_settings,
            } if live_settings == settings
        ));
        assert!(
            fixture
                .host
                .agent_status_snapshot(&agent_id)
                .await
                .expect("active status")
                .activity_counter
                > observation.status.activity_counter
        );
        assert_eq!(
            observation.handle.interrupt().await,
            InterruptOutcome::Interrupted
        );
    }

    #[tokio::test]
    async fn production_scheduler_actor_gate_rejects_stale_settings() {
        let fixture = compact_fixture_without_supervisor_worker().await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorRetryAttempts { count: 0 },
            })
            .await
            .expect("set one-call budget");
        let (agent_id, _) = spawn_idle_user_agent(
            &fixture.host,
            &format!(
                "{} {}",
                crate::backend::mock::MOCK_USER_BUBBLES_SENTINEL,
                crate::agent::supervisor::MOCK_SUPERVISOR_CONTINUE,
            ),
        )
        .await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let stale_settings = fixture.host.supervisor_settings_signal().await;
        assert_eq!(stale_settings.settings.retry_attempts, 0);
        assert_eq!(
            stale_settings.settings.cost_tier,
            protocol::SupervisorCostTier::Low
        );
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("idle observation");
        let now = Instant::now();
        let entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: observation.status.activity_counter,
                phase: SupervisorPhase::Debouncing {
                    idle_since: now.checked_sub(SUPERVISION_DEBOUNCE).unwrap(),
                },
            },
        )]);
        let (verdict_tx, mut verdict_rx) = mpsc::unbounded_channel();
        let (compaction_tx, _compaction_rx) = mpsc::unbounded_channel();
        let semaphore = Arc::new(Semaphore::new(1));
        let (pre_start_entered, pre_start_release) =
            crate::agent::install_begin_supervisor_verdict_test_gate(agent_id.clone());
        let (mut starts, releases) = install_supervisor_verdict_call_test_gate(agent_id.clone());

        let host = fixture.host.clone();
        let task_verdict_tx = verdict_tx.clone();
        let task_compaction_tx = compaction_tx.clone();
        let task_semaphore = Arc::clone(&semaphore);
        let launch = tokio::spawn(async move {
            let mut entries = entries;
            let mut verdict_task_state = SupervisorVerdictTaskState::default();
            process_supervisor_deadlines_at(
                &host,
                &mut entries,
                stale_settings,
                &task_verdict_tx,
                &task_compaction_tx,
                &task_semaphore,
                &mut verdict_task_state,
                now,
            )
            .await;
            (entries, verdict_task_state)
        });
        pre_start_entered
            .await
            .expect("actor reached verdict settings boundary");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorCostTier {
                    tier: protocol::SupervisorCostTier::High,
                },
            })
            .await
            .expect("change verdict settings before actor authorization");
        let live_settings = fixture.host.supervisor_settings_signal().await;
        pre_start_release.send(()).expect("release pre-start gate");
        let (mut entries, mut verdict_task_state) = launch.await.expect("deadline task");

        assert!(!verdict_task_state.is_active());
        assert!(starts.try_recv().is_err(), "stale Low call must be free");
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Debouncing { .. })
        ));
        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            live_settings,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            now,
        )
        .await;
        let start = starts.recv().await.expect("live High call start");
        assert_eq!(start.attempts_started, 1);
        assert_eq!(start.cost_hint, Some(protocol::SpawnCostHint::High));
        releases.send(()).expect("release live call");
        accept_next_supervisor_task_event(
            &fixture.host,
            &mut entries,
            live_settings,
            &mut verdict_task_state,
            &mut verdict_rx,
            Instant::now(),
        )
        .await;
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::Active)
        ));
        let mut kicks = 0;
        for _ in 0..100 {
            kicks = observation
                .handle
                .read_supervision_context()
                .await
                .expect("context after live kick")
                .kicks_since_user_message;
            if kicks == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(kicks, 1);
        remove_supervisor_verdict_call_test_gate(&agent_id);
    }

    #[tokio::test]
    async fn production_scheduler_serializes_due_agents_through_one_task_owner() {
        let fixture = compact_fixture_without_supervisor_worker().await;
        let prompt = format!(
            "{} {}",
            crate::backend::mock::MOCK_USER_BUBBLES_SENTINEL,
            crate::agent::supervisor::MOCK_SUPERVISOR_ERROR,
        );
        let (first_agent, _) = spawn_idle_user_agent(&fixture.host, &prompt).await;
        let (second_agent, _) = spawn_idle_user_agent(&fixture.host, &prompt).await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        let settings = fixture.host.supervisor_settings_signal().await;
        let first_observation = fixture
            .host
            .activity_summary_observation(&first_agent)
            .await
            .expect("first idle observation");
        let second_observation = fixture
            .host
            .activity_summary_observation(&second_agent)
            .await
            .expect("second idle observation");
        let now = Instant::now();
        let idle_since = now.checked_sub(SUPERVISION_DEBOUNCE).unwrap();
        let mut entries = HashMap::from([
            (
                first_agent.clone(),
                SupervisorSchedulerEntry {
                    last_activity_counter: first_observation.status.activity_counter,
                    phase: SupervisorPhase::Debouncing { idle_since },
                },
            ),
            (
                second_agent.clone(),
                SupervisorSchedulerEntry {
                    last_activity_counter: second_observation.status.activity_counter,
                    phase: SupervisorPhase::Debouncing { idle_since },
                },
            ),
        ]);
        let (verdict_tx, mut verdict_rx) = mpsc::unbounded_channel();
        let (compaction_tx, _compaction_rx) = mpsc::unbounded_channel();
        let semaphore = Arc::new(Semaphore::new(1));
        let mut verdict_task_state = SupervisorVerdictTaskState::default();
        let (mut first_starts, first_releases) =
            install_supervisor_verdict_call_test_gate(first_agent.clone());
        let (mut second_starts, second_releases) =
            install_supervisor_verdict_call_test_gate(second_agent.clone());

        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            settings,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            now,
        )
        .await;
        let first_owner = tokio::select! {
            start = first_starts.recv() => {
                assert_eq!(start.expect("first call start").agent_id, first_agent);
                1_u8
            }
            start = second_starts.recv() => {
                assert_eq!(start.expect("second call start").agent_id, second_agent);
                2_u8
            }
        };
        assert!(verdict_task_state.is_active());
        assert_eq!(
            supervisor_next_deadline(&entries, settings, verdict_task_state.is_active()),
            None
        );
        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            settings,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            now,
        )
        .await;
        if first_owner == 1 {
            assert!(second_starts.try_recv().is_err());
            first_releases.send(()).expect("release first owner");
        } else {
            assert!(first_starts.try_recv().is_err());
            second_releases.send(()).expect("release second owner");
        }
        accept_next_supervisor_task_event(
            &fixture.host,
            &mut entries,
            settings,
            &mut verdict_task_state,
            &mut verdict_rx,
            Instant::now(),
        )
        .await;
        process_supervisor_deadlines_at(
            &fixture.host,
            &mut entries,
            settings,
            &verdict_tx,
            &compaction_tx,
            &semaphore,
            &mut verdict_task_state,
            now,
        )
        .await;
        if first_owner == 1 {
            assert_eq!(
                second_starts
                    .recv()
                    .await
                    .expect("second serialized call")
                    .agent_id,
                second_agent
            );
            second_releases.send(()).expect("release second call");
        } else {
            assert_eq!(
                first_starts
                    .recv()
                    .await
                    .expect("first serialized call")
                    .agent_id,
                first_agent
            );
            first_releases.send(()).expect("release first call");
        }
        accept_next_supervisor_task_event(
            &fixture.host,
            &mut entries,
            settings,
            &mut verdict_task_state,
            &mut verdict_rx,
            Instant::now(),
        )
        .await;
        remove_supervisor_verdict_call_test_gate(&first_agent);
        remove_supervisor_verdict_call_test_gate(&second_agent);
    }

    async fn assert_settings_edit_rejects_actor_pending_compaction(
        fixture: &CompactFixture,
        agent_id: &AgentId,
        setting: HostSettingValue,
    ) {
        let status = fixture
            .host
            .agent_status_snapshot(agent_id)
            .await
            .expect("agent status");
        let signal = fixture.host.supervisor_settings_signal().await;
        let (entered, release) =
            crate::agent::install_compact_if_inactive_test_gate(agent_id.clone());
        let host = fixture.host.clone();
        let compact_agent_id = agent_id.clone();
        let compact = tokio::spawn(async move {
            let (stream, _rx) = compact_stream(&compact_agent_id);
            host.compact_agent_if_inactive_in_background(
                compact_agent_id,
                status.activity_counter,
                signal.epoch,
                AgentCompactPayload {
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
        });

        entered
            .await
            .expect("conditional compact reached actor gate");
        fixture
            .host
            .set_setting(SetSettingPayload { setting })
            .await
            .expect("commit supervisor setting while actor gate is held");
        release.send(()).expect("release actor gate");

        assert!(
            !compact
                .await
                .expect("conditional compact task")
                .expect("conditional compact request")
        );
    }

    #[tokio::test]
    async fn actor_gate_rejects_each_stale_supervisor_setting_kind() {
        let fixture = compact_fixture().await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("enable supervisor");
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: true },
            })
            .await
            .expect("enable auto-compaction");
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "actor settings race").await;

        assert_settings_edit_rejects_actor_pending_compaction(
            &fixture,
            &agent_id,
            HostSettingValue::SupervisorEnabled { enabled: false },
        )
        .await;
        fixture
            .host
            .set_setting(SetSettingPayload {
                setting: HostSettingValue::SupervisorEnabled { enabled: true },
            })
            .await
            .expect("restore supervisor");
        assert_settings_edit_rejects_actor_pending_compaction(
            &fixture,
            &agent_id,
            HostSettingValue::SupervisorAutoCompactOnSuccess { enabled: false },
        )
        .await;
        assert_settings_edit_rejects_actor_pending_compaction(
            &fixture,
            &agent_id,
            HostSettingValue::SupervisorAutoCompactInactivityDelaySeconds { seconds: 900 },
        )
        .await;
        assert_settings_edit_rejects_actor_pending_compaction(
            &fixture,
            &agent_id,
            HostSettingValue::SupervisorAutoCompactMinContextTokens { tokens: 900_000 },
        )
        .await;
    }

    #[tokio::test]
    async fn actor_gate_linearizes_activity_before_conditional_compaction() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "activity wins race").await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let expected_counter = observation.status.activity_counter;
        assert!(
            observation
                .handle
                .send_input(AgentInput::SendMessage(SendMessagePayload {
                    message: "new user activity".to_owned(),
                    images: None,
                    origin: Some(MessageOrigin::User),
                    tool_response: None,
                }))
                .await
        );
        let signal = fixture.host.supervisor_settings_signal().await;
        let (stream, _rx) = compact_stream(&agent_id);

        let accepted = fixture
            .host
            .compact_agent_if_inactive_in_background(
                agent_id,
                expected_counter,
                signal.epoch,
                AgentCompactPayload {
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
            .expect("conditional compact request");

        assert!(!accepted);
    }

    #[tokio::test]
    async fn actor_gate_accepts_compaction_that_linearizes_first() {
        let fixture = compact_fixture().await;
        let (agent_id, _) = spawn_idle_user_agent(&fixture.host, "compaction wins race").await;
        let observation = fixture
            .host
            .activity_summary_observation(&agent_id)
            .await
            .expect("agent observation");
        let expected_counter = observation.status.activity_counter;
        let agent_handle = observation.handle;
        let signal = fixture.host.supervisor_settings_signal().await;
        let (entered, release) =
            crate::agent::install_compact_if_inactive_test_gate(agent_id.clone());
        let host = fixture.host.clone();
        let compact_agent_id = agent_id.clone();
        let compact = tokio::spawn(async move {
            let (stream, _rx) = compact_stream(&compact_agent_id);
            host.compact_agent_if_inactive_in_background(
                compact_agent_id,
                expected_counter,
                signal.epoch,
                AgentCompactPayload {
                    summary_prompt: None,
                    max_summary_bytes: None,
                },
                stream,
            )
            .await
        });
        entered
            .await
            .expect("conditional compact reached actor gate");
        release.send(()).expect("release actor gate");
        assert!(
            compact
                .await
                .expect("conditional compact task")
                .expect("conditional compact request")
        );

        assert!(
            agent_handle
                .send_input(AgentInput::SendMessage(SendMessagePayload {
                    message: "activity after compaction acceptance".to_owned(),
                    images: None,
                    origin: Some(MessageOrigin::User),
                    tool_response: None,
                }))
                .await
        );
    }

    #[test]
    fn supervisor_retry_limit_changes_preserve_or_exhaust_pending_retry() {
        let idle_since = Instant::now();
        let due_at = idle_since.checked_add(Duration::from_secs(30)).unwrap();
        let baseline = SupervisionBaseline {
            last_user_message: "request".to_owned(),
            kicks_since_user_message: 0,
            session_id: None,
        };
        let mut pending = SupervisorSchedulerEntry {
            last_activity_counter: 4,
            phase: SupervisorPhase::RetryPending {
                idle_since,
                baseline: baseline.clone(),
                attempts_started: 1,
                due_at,
                last_failure_kind: SupervisionRetryReason::Failure(
                    crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                ),
                verdict_settings: VerdictSettingsFingerprint::from(
                    protocol::SupervisorSettings::default(),
                ),
            },
        };
        let mut raised = protocol::SupervisorSettings::default();
        raised.retry_attempts = 5;
        raised.cost_tier = protocol::SupervisorCostTier::High;
        apply_live_retry_settings(&AgentId("retry-limit".to_owned()), &mut pending, raised);
        assert!(matches!(
            pending.phase,
            SupervisorPhase::RetryPending {
                due_at: preserved_due,
                attempts_started: 1,
                verdict_settings,
                ..
            } if preserved_due == due_at
                && verdict_settings == VerdictSettingsFingerprint::from(raised)
        ));

        let mut lowered = raised;
        lowered.retry_attempts = 0;
        assert!(matches!(
            apply_live_retry_settings(&AgentId("retry-limit".to_owned()), &mut pending, lowered),
            LiveRetrySettingsResult::FailureExhausted {
                attempts_started: 1,
                ..
            }
        ));
        assert!(matches!(
            pending.phase,
            SupervisorPhase::FailureExhausted { .. }
        ));
    }

    #[test]
    fn supervisor_retry_backoff_and_caps_are_exact_and_finite() {
        let agent_id = AgentId("retry-backoff".to_owned());
        let idle_since = Instant::now();
        let baseline = SupervisionBaseline {
            last_user_message: "retry".to_owned(),
            kicks_since_user_message: 0,
            session_id: None,
        };
        let mut settings = protocol::SupervisorSettings::default();
        settings.enabled = true;
        settings.retry_attempts = 5;
        assert_eq!(settings.retry_attempts.saturating_add(1), 6);
        let mut entries = HashMap::from([(
            agent_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: 1,
                phase: SupervisorPhase::Dormant { idle_since },
            },
        )]);

        for attempts_started in 1..=5 {
            let scheduled_at = idle_since
                .checked_add(Duration::from_secs(u64::from(attempts_started)))
                .unwrap();
            assert_eq!(
                schedule_supervision_retry_at(
                    &mut entries,
                    &agent_id,
                    idle_since,
                    baseline.clone(),
                    attempts_started,
                    settings,
                    SupervisionRetryReason::Failure(
                        crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    ),
                    Some("outage".to_owned()),
                    scheduled_at,
                ),
                SupervisionRetrySchedule::Pending
            );
            let expected_due = scheduled_at
                .checked_add(SUPERVISION_RETRY_DELAYS[usize::from(attempts_started - 1)])
                .unwrap();
            assert!(matches!(
                entries.get(&agent_id).map(|entry| &entry.phase),
                Some(SupervisorPhase::RetryPending {
                    due_at,
                    attempts_started: actual,
                    ..
                }) if *due_at == expected_due && *actual == attempts_started
            ));
            for _ in 0..100 {
                assert_eq!(
                    supervisor_next_deadline(
                        &entries,
                        SupervisorSettingsSignal { settings, epoch: 1 },
                        false,
                    ),
                    Some(expected_due),
                    "polling before a retry transition must not move its deadline"
                );
            }
        }

        assert_eq!(
            schedule_supervision_retry_at(
                &mut entries,
                &agent_id,
                idle_since,
                baseline.clone(),
                6,
                settings,
                SupervisionRetryReason::Failure(
                    crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                ),
                Some("outage".to_owned()),
                Instant::now(),
            ),
            SupervisionRetrySchedule::Exhausted
        );
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::FailureExhausted { .. })
        ));
        for _ in 0..100 {
            assert_eq!(
                supervisor_next_deadline(
                    &entries,
                    SupervisorSettingsSignal { settings, epoch: 1 },
                    false,
                ),
                None,
                "failure-exhausted warning gating must never create an immediate deadline"
            );
        }

        settings.retry_attempts = 1;
        assert_eq!(settings.retry_attempts.saturating_add(1), 2);
        assert_eq!(
            schedule_supervision_retry_at(
                &mut entries,
                &agent_id,
                idle_since,
                baseline.clone(),
                1,
                settings,
                SupervisionRetryReason::Failure(
                    crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                ),
                None,
                idle_since,
            ),
            SupervisionRetrySchedule::Pending
        );
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::RetryPending {
                attempts_started: 1,
                due_at,
                ..
            }) if *due_at == idle_since.checked_add(Duration::from_secs(30)).unwrap()
        ));
        assert_eq!(
            schedule_supervision_retry_at(
                &mut entries,
                &agent_id,
                idle_since,
                baseline.clone(),
                2,
                settings,
                SupervisionRetryReason::Failure(
                    crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                ),
                None,
                idle_since,
            ),
            SupervisionRetrySchedule::Exhausted
        );
        assert!(matches!(
            entries.get(&agent_id).map(|entry| &entry.phase),
            Some(SupervisorPhase::FailureExhausted {
                retry_due_at: Some(_),
                ..
            })
        ));
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal { settings, epoch: 1 },
                false,
            ),
            None
        );

        settings.retry_attempts = 0;
        assert_eq!(settings.retry_attempts.saturating_add(1), 1);
        assert_eq!(
            schedule_supervision_retry_at(
                &mut entries,
                &agent_id,
                idle_since,
                baseline,
                1,
                settings,
                SupervisionRetryReason::Failure(
                    crate::agent::supervisor::SupervisionFailureKind::InvalidVerdict,
                ),
                None,
                Instant::now(),
            ),
            SupervisionRetrySchedule::Exhausted
        );
    }

    #[test]
    fn supervisor_retry_deadlines_serialize_and_each_agent_stops_at_default_cap() {
        let now = Instant::now();
        let mut settings = protocol::SupervisorSettings::default();
        settings.enabled = true;
        assert_eq!(settings.retry_attempts.saturating_add(1), 2);
        let fingerprint = VerdictSettingsFingerprint::from(settings);
        let baseline = || SupervisionBaseline {
            last_user_message: "retry".to_owned(),
            kicks_since_user_message: 0,
            session_id: None,
        };
        let mut entries = HashMap::new();
        for index in 0..3 {
            entries.insert(
                AgentId(format!("retry-agent-{index}")),
                SupervisorSchedulerEntry {
                    last_activity_counter: 1,
                    phase: SupervisorPhase::RetryPending {
                        idle_since: now,
                        baseline: baseline(),
                        attempts_started: 1,
                        due_at: now,
                        last_failure_kind: SupervisionRetryReason::Failure(
                            crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                        ),
                        verdict_settings: fingerprint,
                    },
                },
            );
        }
        let active_id = AgentId("active-verdict".to_owned());
        entries.insert(
            active_id.clone(),
            SupervisorSchedulerEntry {
                last_activity_counter: 1,
                phase: SupervisorPhase::VerdictInFlight {
                    idle_since: now,
                    baseline: baseline(),
                    attempts_started: 1,
                    verdict_settings: fingerprint,
                },
            },
        );
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal { settings, epoch: 1 },
                true,
            ),
            None,
            "one in-flight call must suppress all due retry launches"
        );
        entries.remove(&active_id);
        assert_eq!(
            supervisor_next_deadline(
                &entries,
                SupervisorSettingsSignal { settings, epoch: 1 },
                false,
            ),
            Some(now)
        );

        let ids = entries.keys().cloned().collect::<Vec<_>>();
        for agent_id in ids {
            assert_eq!(
                schedule_supervision_retry_at(
                    &mut entries,
                    &agent_id,
                    now,
                    baseline(),
                    2,
                    settings,
                    SupervisionRetryReason::Failure(
                        crate::agent::supervisor::SupervisionFailureKind::BackendStream,
                    ),
                    Some("outage".to_owned()),
                    now,
                ),
                SupervisionRetrySchedule::Exhausted
            );
        }
    }
}
