use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::bridge::{ConfiguredHost, RemoteHostLifecycleStatus};
use leptos::prelude::*;
use protocol::FrameKind;
use protocol::{
    AgentActivityStats, AgentActivitySummaryState, AgentGroupMode, AgentId, AgentListDensity,
    AgentOrderKey, AgentOrigin, AgentSortMode, AgentWorkflowMetadata, AgentsSidebarPreferences,
    AgentsViewFilters, AgentsViewPreferences, AgentsViewPreferencesSnapshot, BackendKind,
    BackendSetupInfo, ByteRange, ChatMessage, ChatMessageId, CodeIntelDiagnostic,
    CodeIntelErrorPayload, CodeIntelFileModelPayload, CodeIntelLocation, CodeIntelOccurrence,
    CodeIntelOverviewPayload, CodeIntelReferencesFileResult, CodeIntelStatusPayload,
    CompactionObservationId, CompactionOperationId, ContextCompactionNotifyPayload,
    ContextCompactionStatus, ContextCompactionTimelineEvent, CustomAgent, CustomAgentId,
    DiffContextMode, GitBranchName, HistoryPageRequestId, HostAbsPath, HostBrowseEntry,
    HostBrowseErrorPayload, HostPlatform, LaunchProfileCatalog, LaunchProfileId, McpServerConfig,
    McpServerId, MessageMetadataUpdateData, MobileAccessStatePayload, MobilePairingOfferPayload,
    Project, ProjectDiffScope, ProjectFileVersion, ProjectGitDiffFile, ProjectGitDiffPayload,
    ProjectId, ProjectPath, ProjectRootGitStatus, ProjectRootListing, ProjectRootPath,
    ProjectSearchFileResult, QueuedMessageEntry, RequestedCompactionAvailability, Review,
    ReviewCommentId, ReviewId, ReviewSuggestionId, ReviewSummary, SessionId, SessionSchemaEntry,
    SessionSettingsValues, SessionSummary, Skill, SkillId, SmartViewId, Steering, SteeringId,
    StreamPath, TaskList, TaskTokenUsagePayload, Team, TeamDraft, TeamDraftId, TeamId, TeamMember,
    TeamMemberBindingPayload, TeamMemberId, TeamMemberShuffleSuggestion,
    TeamMemberShuffleSuggestionNotifyPayload, TeamPresetCatalog, TerminalId,
    ToolExecutionCompletedData, ToolProgressData, ToolRequest, WorkflowCatalogLocation,
    WorkflowDiagnostic, WorkflowId, WorkflowInputSpec, WorkflowRunId, WorkflowRunSnapshot,
    WorkflowSummary,
};
use settings_model::HostSettings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffViewMode {
    Unified,
    SideBySide,
}

/// How verbose tool-call cards render in the chat.
///
/// `Summary` collapses the body to header-only; `Compact` shows previews with
/// per-tool caps and an expand toggle; `Full` shows everything inline.
/// Persisted to `localStorage` via `persist_tool_output_mode` —
/// pure presentation, never sent over the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutputMode {
    Summary,
    Compact,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// In-flight/failed state for a backend-native settings save. See
/// [`AppState::native_settings_save_state`].
#[derive(Clone, Debug, PartialEq)]
pub enum NativeSettingsSaveState {
    /// A save is in flight. `base` is the settings document the save was applied
    /// to; the save is considered landed once the server publishes a snapshot
    /// whose settings document differs from `base`.
    Pending {
        base: serde_json::Value,
        write_id: protocol::SettingsWriteId,
    },
    /// The last save failed to send; carries a user-facing reason.
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub host_id: String,
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub team_member_id: Option<TeamMemberId>,
    pub session_id: Option<SessionId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub workflow: Option<AgentWorkflowMetadata>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    pub started: bool,
    /// Set when a fatal `AgentError` arrives. The agent is terminated and no
    /// further events will arrive on its stream.
    pub fatal_error: Option<String>,
    /// Server-owned background activity summary state. Rendered (when enabled)
    /// in surfaces like the await-agents tool card. Defaults to `Disabled`;
    /// the frontend never infers this — it mirrors server-emitted state from
    /// `NewAgentPayload.activity_summary` and `AgentActivitySummary` frames.
    pub activity_summary: AgentActivitySummaryState,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentMonitorKey {
    pub host_id: String,
    pub agent_id: AgentId,
}

impl AgentMonitorKey {
    pub fn new(host_id: impl Into<String>, agent_id: AgentId) -> Self {
        Self {
            host_id: host_id.into(),
            agent_id,
        }
    }

    pub fn from_agent(agent: &AgentInfo) -> Self {
        Self::new(agent.host_id.clone(), agent.agent_id.clone())
    }
}

// ── Tab system ──────────────────────────────────────────────────────────

/// Maximum number of tab content components mounted at once. The active tab
/// is always mounted; the rest of the slots hold the most-recently-active
/// tabs before it (display:none, but state preserved for instant switch
/// back). Tabs beyond this hot set are fully unmounted — switching back
/// remounts them from cached AppState (chat_rows, open_files, diff_contents)
/// so no data is lost, only ephemeral UI state like scroll position.
pub const TAB_LRU_CAPACITY: usize = 2;
pub const CENTER_SPLIT_RATIO_STORAGE_KEY: &str = "tyde-center-split-ratio";
#[cfg(target_arch = "wasm32")]
const ACTIVE_PROJECT_STORAGE_KEY: &str = "tyde-active-project";
#[cfg(target_arch = "wasm32")]
const COMPOSER_DRAFT_STORAGE_KEY: &str = "tyde-composer-draft";
#[cfg(target_arch = "wasm32")]
const MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES: usize = 8;
#[cfg(target_arch = "wasm32")]
const MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES: usize = 256 * 1024;
#[cfg(target_arch = "wasm32")]
const MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES: usize = 512 * 1024;
#[cfg(target_arch = "wasm32")]
const COMPOSER_DRAFT_DEBOUNCE_MS: i32 = 400;
/// Deliberately a second key rather than an extension of the project record:
/// the project format already ships and is covered by tests, and a separate
/// key means an old or corrupt selection degrades to today's behaviour instead
/// of taking project restoration down with it.
///
/// Gated like [`ACTIVE_PROJECT_STORAGE_KEY`]: only the wasm persistence
/// functions read it, and `localStorage` does not exist off the browser.
#[cfg(target_arch = "wasm32")]
const WORKSPACE_SELECTION_STORAGE_KEY: &str = "tyde-workspace-selection";

/// Id of the builtin "Default" custom agent. It backs every spawn that picks
/// no explicit agent, so pickers that already offer a "Default agent" row
/// hide this record to avoid a duplicate entry.
pub const DEFAULT_CUSTOM_AGENT_ID: &str = "tyde-default";

/// Configured-connection id of the primary local host. It is the only host
/// that owns and emits Agents-view preferences (dev-docs/26 §12.1); a `Some`
/// snapshot from any other host is ignored so a stray remote payload cannot
/// hijack the client-global preference signal or its owner pointer.
pub const PRIMARY_LOCAL_HOST_ID: &str = "local";

/// Safety backstop: if an optimistic Agents-view overlay is not reconciled by
/// an authoritative server snapshot within this window (e.g. the
/// `SetAgentsViewPreferences` send was dropped and no notify ever arrives), it
/// is dropped so a failed mutation can never freeze the view.
#[cfg(target_arch = "wasm32")]
const OVERLAY_RECONCILE_TIMEOUT_MS: i32 = 4000;

thread_local! {
    static NEXT_TAB_ID: Cell<u64> = const { Cell::new(0) };
}

#[cfg(target_arch = "wasm32")]
fn load_active_project() -> Option<ActiveProjectRef> {
    let storage = web_sys::window()?.local_storage().ok().flatten()?;
    let encoded = storage
        .get_item(ACTIVE_PROJECT_STORAGE_KEY)
        .ok()
        .flatten()?;
    match serde_json::from_str(&encoded) {
        Ok(project) => Some(project),
        Err(error) => {
            log::warn!("invalid persisted active project: {error}");
            let _ = storage.remove_item(ACTIVE_PROJECT_STORAGE_KEY);
            None
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_active_project() -> Option<ActiveProjectRef> {
    None
}

#[cfg(target_arch = "wasm32")]
fn persist_active_project(project: Option<&ActiveProjectRef>) {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return;
    };
    match project {
        Some(project) => match serde_json::to_string(project) {
            Ok(encoded) => {
                if let Err(error) = storage.set_item(ACTIVE_PROJECT_STORAGE_KEY, &encoded) {
                    log::warn!("failed to persist active project: {error:?}");
                }
            }
            Err(error) => log::warn!("failed to encode active project: {error}"),
        },
        None => {
            let _ = storage.remove_item(ACTIVE_PROJECT_STORAGE_KEY);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_active_project(_project: Option<&ActiveProjectRef>) {}

/// Key for accumulated session-list paging state.
///
/// `SessionListScope` is not `Hash`, so the scope travels as a stable
/// discriminant rather than the enum itself; the pair is still what identifies
/// one paged result set.
pub type SessionListPageKey = (String, &'static str);

/// Stable discriminant for a scope, for use in [`SessionListPageKey`].
pub fn session_list_scope_key(scope: protocol::SessionListScope) -> &'static str {
    match scope {
        protocol::SessionListScope::RootSessions => "root_sessions",
        protocol::SessionListScope::AllSessions => "all_sessions",
    }
}

/// The chat that was open, with enough identity to prove it is still the *same*
/// chat in the same place on the way back in.
///
/// `agent_id` alone is not an identity: an agent id that happens to exist says
/// nothing about which project owns it, so restoring on that alone can drop a
/// project-A chat into project B.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedChatRef {
    pub host_id: String,
    /// Live-instance accelerator: which agent was rendering the chat.
    pub agent_id: AgentId,
    /// Project that owned the chat, or `None` for a Home chat. Restoration
    /// refuses to open the chat into a different project.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// The backend session the chat was showing, once known. This is the
    /// stable half of the identity: an agent id can be reused by a different
    /// session in the same project, so matching on the agent alone proves only
    /// that *an* agent exists, not that it is the conversation the user left.
    /// `None` while the agent has not been assigned a session yet.
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedComposerDraftOwner {
    ActiveChat {
        host_id: String,
        agent_id: AgentId,
        #[serde(default)]
        project_id: Option<ProjectId>,
        #[serde(default)]
        session_id: Option<SessionId>,
    },
    NewChat {
        host_id: String,
        #[serde(default)]
        project_id: Option<ProjectId>,
    },
    TeamMember {
        host_id: String,
        #[serde(default)]
        project_id: Option<ProjectId>,
        member_id: TeamMemberId,
    },
}

#[cfg(target_arch = "wasm32")]
impl PersistedComposerDraftOwner {
    fn same_conversation(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ActiveChat {
                    host_id,
                    agent_id,
                    project_id,
                    session_id,
                },
                Self::ActiveChat {
                    host_id: other_host,
                    agent_id: other_agent,
                    project_id: other_project,
                    session_id: other_session,
                },
            ) => {
                host_id == other_host
                    && agent_id == other_agent
                    && project_id == other_project
                    && (session_id == other_session
                        || session_id.is_none()
                        || other_session.is_none())
            }
            _ => self == other,
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedComposerDraft {
    owner: PersistedComposerDraftOwner,
    text: String,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedComposerDraftStore {
    #[serde(default)]
    entries: Vec<PersistedComposerDraft>,
    #[serde(skip)]
    total_text_bytes: usize,
}

#[cfg(all(test, target_arch = "wasm32"))]
thread_local! {
    static COMPOSER_DRAFT_SERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static COMPOSER_DRAFT_LIMIT_NOTICES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(target_arch = "wasm32")]
fn record_composer_draft_serialization() {
    #[cfg(test)]
    COMPOSER_DRAFT_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(target_arch = "wasm32")]
fn serialized_composer_draft_len(draft: &PersistedComposerDraft) -> Option<usize> {
    record_composer_draft_serialization();
    serde_json::to_vec(draft).ok().map(|encoded| encoded.len())
}

#[cfg(target_arch = "wasm32")]
fn serialize_composer_draft_store(
    drafts: &PersistedComposerDraftStore,
) -> Result<String, serde_json::Error> {
    record_composer_draft_serialization();
    serde_json::to_string(drafts)
}

#[cfg(target_arch = "wasm32")]
impl PersistedComposerDraftStore {
    fn find_index(&self, owner: &PersistedComposerDraftOwner) -> Option<usize> {
        self.entries
            .iter()
            .position(|draft| draft.owner.same_conversation(owner))
    }

    fn restore(&mut self, owner: &PersistedComposerDraftOwner) -> Option<String> {
        let index = self.find_index(owner)?;
        let mut draft = self.entries.remove(index);
        draft.owner = owner.clone();
        let text = draft.text.clone();
        self.entries.insert(0, draft);
        Some(text)
    }

    fn remove(&mut self, owner: &PersistedComposerDraftOwner) -> bool {
        let Some(index) = self.find_index(owner) else {
            return false;
        };
        let removed = self.entries.remove(index);
        self.total_text_bytes = self.total_text_bytes.saturating_sub(removed.text.len());
        true
    }

    fn upsert(&mut self, owner: PersistedComposerDraftOwner, text: String) -> DraftStoreUpdate {
        self.remove(&owner);
        if text.len() >= MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES {
            return DraftStoreUpdate::EntryTooLarge;
        }

        self.total_text_bytes += text.len();
        self.entries
            .insert(0, PersistedComposerDraft { owner, text });
        let mut evicted = 0;
        while self.entries.len() > MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES
            || self.total_text_bytes > MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES
        {
            self.pop_lru();
            evicted += 1;
        }
        DraftStoreUpdate::Stored { evicted }
    }

    fn encoded_len(&self) -> Result<usize, serde_json::Error> {
        record_composer_draft_serialization();
        serde_json::to_vec(self).map(|encoded| encoded.len())
    }

    fn pop_lru(&mut self) {
        if let Some(removed) = self.entries.pop() {
            self.total_text_bytes = self.total_text_bytes.saturating_sub(removed.text.len());
        }
    }

    fn enforce_tracked_bounds(&mut self) -> usize {
        let before_retain = self.entries.len();
        self.entries.retain(|draft| {
            !draft.text.is_empty() && draft.text.len() < MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES
        });
        let mut evicted = before_retain - self.entries.len();
        self.total_text_bytes = self.entries.iter().map(|draft| draft.text.len()).sum();

        let before_truncate = self.entries.len();
        self.entries.truncate(MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES);
        evicted += before_truncate - self.entries.len();
        self.total_text_bytes = self.entries.iter().map(|draft| draft.text.len()).sum();

        while self.total_text_bytes > MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES {
            self.pop_lru();
            evicted += 1;
        }
        evicted
    }

    fn finalize_bounds(
        &mut self,
        active_owner: Option<&PersistedComposerDraftOwner>,
    ) -> DraftBoundsOutcome {
        let mut outcome = DraftBoundsOutcome {
            evicted: self.enforce_tracked_bounds(),
            ..DraftBoundsOutcome::default()
        };
        self.entries.retain(|draft| {
            let keep = match serialized_composer_draft_len(draft) {
                Some(bytes) => bytes <= MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES,
                None => {
                    outcome.failure = Some(DraftPersistenceFailure::Encoding);
                    false
                }
            };
            if !keep {
                outcome.evicted += 1;
                outcome.active_owner_too_large |=
                    active_owner.is_some_and(|owner| draft.owner.same_conversation(owner));
            }
            keep
        });
        self.total_text_bytes = self.entries.iter().map(|draft| draft.text.len()).sum();

        loop {
            match self.encoded_len() {
                Ok(bytes) if bytes <= MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES => break,
                Ok(_) if !self.entries.is_empty() => {
                    self.pop_lru();
                    outcome.evicted += 1;
                }
                Ok(_) => {
                    outcome.failure = Some(DraftPersistenceFailure::ExactSizeBackstop);
                    break;
                }
                Err(_) => {
                    outcome.failure = Some(DraftPersistenceFailure::Encoding);
                    break;
                }
            }
        }
        outcome
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DraftStoreUpdate {
    Stored { evicted: usize },
    EntryTooLarge,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DraftBoundsOutcome {
    evicted: usize,
    active_owner_too_large: bool,
    failure: Option<DraftPersistenceFailure>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DraftPersistenceFailure {
    Encoding,
    ExactSizeBackstop,
    StorageWrite,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DraftPersistenceOutcome {
    bounds: DraftBoundsOutcome,
    active_owner_persistable: bool,
    failure: Option<DraftPersistenceFailure>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct ComposerDraftScheduler {
    schedule: std::rc::Rc<dyn Fn(&js_sys::Function, i32) -> Result<i32, wasm_bindgen::JsValue>>,
    cancel: std::rc::Rc<dyn Fn(i32)>,
}

#[cfg(target_arch = "wasm32")]
impl Default for ComposerDraftScheduler {
    fn default() -> Self {
        Self {
            schedule: std::rc::Rc::new(|callback, delay_ms| {
                web_sys::window()
                    .ok_or_else(|| wasm_bindgen::JsValue::from_str("window is unavailable"))?
                    .set_timeout_with_callback_and_timeout_and_arguments_0(callback, delay_ms)
            }),
            cancel: std::rc::Rc::new(|handle| {
                if let Some(window) = web_sys::window() {
                    window.clear_timeout_with_handle(handle);
                }
            }),
        }
    }
}

#[cfg(target_arch = "wasm32")]
struct ComposerDraftTimeout {
    handle: i32,
    _callback: wasm_bindgen::closure::Closure<dyn FnMut()>,
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static NEXT_COMPOSER_DRAFT_SCHEDULER_ID: Cell<u64> = const { Cell::new(0) };
    static COMPOSER_DRAFT_SCHEDULERS: std::cell::RefCell<HashMap<u64, ComposerDraftScheduler>> =
        std::cell::RefCell::new(HashMap::new());
    static COMPOSER_DRAFT_TIMEOUTS: std::cell::RefCell<HashMap<u64, ComposerDraftTimeout>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(target_arch = "wasm32")]
struct ComposerDraftPersistenceRegistration {
    id: u64,
}

#[cfg(target_arch = "wasm32")]
impl Drop for ComposerDraftPersistenceRegistration {
    fn drop(&mut self) {
        let timeout =
            COMPOSER_DRAFT_TIMEOUTS.with(|timeouts| timeouts.borrow_mut().remove(&self.id));
        let scheduler =
            COMPOSER_DRAFT_SCHEDULERS.with(|schedulers| schedulers.borrow_mut().remove(&self.id));
        if let (Some(timeout), Some(scheduler)) = (timeout, scheduler) {
            (scheduler.cancel)(timeout.handle);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn register_composer_draft_scheduler() -> Arc<ComposerDraftPersistenceRegistration> {
    let id = NEXT_COMPOSER_DRAFT_SCHEDULER_ID.with(|next| {
        let id = next.get().wrapping_add(1);
        next.set(id);
        id
    });
    COMPOSER_DRAFT_SCHEDULERS.with(|schedulers| {
        schedulers
            .borrow_mut()
            .insert(id, ComposerDraftScheduler::default());
    });
    Arc::new(ComposerDraftPersistenceRegistration { id })
}

#[cfg(target_arch = "wasm32")]
fn load_composer_drafts() -> PersistedComposerDraftStore {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return PersistedComposerDraftStore::default();
    };
    let Some(encoded) = storage.get_item(COMPOSER_DRAFT_STORAGE_KEY).ok().flatten() else {
        return PersistedComposerDraftStore::default();
    };
    let mut drafts = match serde_json::from_str::<PersistedComposerDraftStore>(&encoded) {
        Ok(drafts) => drafts,
        Err(error) => {
            log::warn!("invalid persisted composer draft store: {error}");
            let _ = storage.remove_item(COMPOSER_DRAFT_STORAGE_KEY);
            return PersistedComposerDraftStore::default();
        }
    };
    drafts.enforce_tracked_bounds();
    drafts
}

#[cfg(target_arch = "wasm32")]
fn persist_composer_drafts(
    drafts: &mut PersistedComposerDraftStore,
    active_owner: Option<&PersistedComposerDraftOwner>,
) -> DraftPersistenceOutcome {
    let bounds = drafts.finalize_bounds(active_owner);
    let mut outcome = DraftPersistenceOutcome {
        failure: bounds.failure,
        bounds,
        ..DraftPersistenceOutcome::default()
    };
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        log::warn!("composer draft storage is unavailable");
        outcome.failure = Some(DraftPersistenceFailure::StorageWrite);
        return outcome;
    };
    if drafts.entries.is_empty() {
        if let Err(error) = storage.remove_item(COMPOSER_DRAFT_STORAGE_KEY) {
            log::warn!("failed to clear composer draft store: {error:?}");
            outcome.failure = Some(DraftPersistenceFailure::StorageWrite);
        }
        return outcome;
    }
    match serialize_composer_draft_store(drafts) {
        Ok(encoded) => {
            if encoded.len() > MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES {
                log::error!("bounded composer draft store exceeded its total limit");
                let _ = storage.remove_item(COMPOSER_DRAFT_STORAGE_KEY);
                outcome.failure = Some(DraftPersistenceFailure::ExactSizeBackstop);
                return outcome;
            }
            if let Err(error) = storage.set_item(COMPOSER_DRAFT_STORAGE_KEY, &encoded) {
                log::warn!("failed to persist composer draft store: {error:?}");
                outcome.failure = Some(DraftPersistenceFailure::StorageWrite);
            }
        }
        Err(error) => {
            log::warn!("failed to encode composer draft store: {error}");
            let _ = storage.remove_item(COMPOSER_DRAFT_STORAGE_KEY);
            outcome.failure = Some(DraftPersistenceFailure::Encoding);
        }
    }
    outcome.active_owner_persistable = outcome.failure.is_none()
        && active_owner.is_some_and(|owner| drafts.find_index(owner).is_some());
    outcome
}

#[cfg(target_arch = "wasm32")]
fn notify_composer_draft_limit(notified: RwSignal<bool>) {
    if notified.get_untracked() {
        return;
    }
    notified.set(true);
    #[cfg(test)]
    COMPOSER_DRAFT_LIMIT_NOTICES.with(|count| count.set(count.get() + 1));
    #[cfg(not(test))]
    wasm_bindgen_futures::spawn_local(async {
        crate::bridge::message_dialog(
            "Draft is too large to protect",
            "This draft exceeds Tyde's crash-recovery storage limit after safe encoding. Shorten it before relying on automatic draft recovery.",
        )
        .await;
    });
}

#[cfg(target_arch = "wasm32")]
fn notify_composer_draft_eviction(notified: RwSignal<bool>, evicted: usize) {
    if evicted == 0 || notified.get_untracked() {
        return;
    }
    notified.set(true);
    wasm_bindgen_futures::spawn_local(async {
        crate::bridge::message_dialog(
            "Draft recovery limit reached",
            "Tyde reached its bounded crash-recovery storage limit. Older drafts were removed from recovery storage.",
        )
        .await;
    });
}

#[cfg(target_arch = "wasm32")]
fn notify_composer_draft_persistence_failure(
    notified: RwSignal<bool>,
    failure: DraftPersistenceFailure,
) {
    log::error!("composer draft persistence failed: {failure:?}");
    if notified.get_untracked() {
        return;
    }
    notified.set(true);
    wasm_bindgen_futures::spawn_local(async {
        crate::bridge::message_dialog(
            "Draft recovery could not be updated",
            "Tyde could not safely update its bounded crash-recovery storage. Your current draft remains visible, but it may not be available after a reload.",
        )
        .await;
    });
}

#[cfg(target_arch = "wasm32")]
fn surface_composer_draft_persistence_outcome(
    pending_evictions: RwSignal<usize>,
    eviction_notified: RwSignal<bool>,
    limit_notified: RwSignal<bool>,
    failure_notified: RwSignal<bool>,
    outcome: DraftPersistenceOutcome,
) {
    let pending = pending_evictions
        .try_update(std::mem::take)
        .unwrap_or_default();
    let evicted = pending.saturating_add(outcome.bounds.evicted);
    if evicted > 0 {
        log::warn!("composer draft bounds evicted {evicted} entries");
    }
    if let Some(failure) = outcome.failure {
        notify_composer_draft_persistence_failure(failure_notified, failure);
    } else if outcome.bounds.active_owner_too_large {
        notify_composer_draft_limit(limit_notified);
    } else {
        if outcome.active_owner_persistable {
            limit_notified.set(false);
        }
        failure_notified.set(false);
        notify_composer_draft_eviction(eviction_notified, evicted);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod composer_draft_tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::*;

    fn owner(host_id: &str) -> PersistedComposerDraftOwner {
        PersistedComposerDraftOwner::NewChat {
            host_id: host_id.to_owned(),
            project_id: None,
        }
    }

    fn clear_storage() {
        web_sys::window()
            .unwrap()
            .local_storage()
            .unwrap()
            .unwrap()
            .remove_item(COMPOSER_DRAFT_STORAGE_KEY)
            .unwrap();
    }

    #[wasm_bindgen_test]
    fn draft_store_evicts_the_least_recently_used_entry_by_count_and_total_bytes() {
        COMPOSER_DRAFT_SERIALIZATIONS.with(|count| count.set(0));
        let mut drafts = PersistedComposerDraftStore::default();
        for index in 0..=MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES {
            assert!(matches!(
                drafts.upsert(owner(&format!("host-{index}")), format!("draft-{index}")),
                DraftStoreUpdate::Stored { .. }
            ));
        }

        assert_eq!(drafts.entries.len(), MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES);
        assert!(drafts.find_index(&owner("host-0")).is_none());
        let newest = format!("host-{MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES}");
        let newest_text = format!("draft-{MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES}");
        assert_eq!(
            drafts.restore(&owner(&newest)).as_deref(),
            Some(newest_text.as_str()),
            "the newest entry survives count-based eviction"
        );

        let mut byte_bounded = PersistedComposerDraftStore::default();
        for index in 0..3 {
            assert!(matches!(
                byte_bounded.upsert(owner(&format!("large-{index}")), "x".repeat(220 * 1024)),
                DraftStoreUpdate::Stored { .. }
            ));
        }
        assert_eq!(
            byte_bounded.entries.len(),
            2,
            "the total-byte ceiling evicts the least-recent entry"
        );
        assert_eq!(byte_bounded.total_text_bytes, 440 * 1024);
        assert!(byte_bounded.find_index(&owner("large-0")).is_none());
        COMPOSER_DRAFT_SERIALIZATIONS.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "keystroke-time upserts must not serialize entries or the store"
            );
        });
        assert!(
            byte_bounded
                .encoded_len()
                .is_ok_and(|bytes| bytes <= MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES)
        );
        assert_eq!(
            byte_bounded.upsert(
                owner("too-large"),
                "x".repeat(MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES)
            ),
            DraftStoreUpdate::EntryTooLarge
        );
        assert!(byte_bounded.find_index(&owner("too-large")).is_none());
    }

    #[wasm_bindgen_test]
    fn exact_persisted_bounds_include_json_escaping_overhead() {
        clear_storage();
        let storage = web_sys::window().unwrap().local_storage().unwrap().unwrap();
        storage
            .set_item(COMPOSER_DRAFT_STORAGE_KEY, "stale")
            .unwrap();
        let escaped_text = "\u{0001}".repeat(50 * 1024);
        let mut entry_bounded = PersistedComposerDraftStore::default();
        assert_eq!(
            entry_bounded.upsert(owner("escaped-active"), escaped_text),
            DraftStoreUpdate::Stored { evicted: 0 },
            "the typing path applies only its cheap raw-byte preflight"
        );
        let escaped_active = owner("escaped-active");
        let entry_outcome = persist_composer_drafts(&mut entry_bounded, Some(&escaped_active));
        assert_eq!(entry_outcome.bounds.evicted, 1);
        assert!(entry_outcome.bounds.active_owner_too_large);
        assert!(!entry_outcome.active_owner_persistable);
        assert_eq!(entry_outcome.failure, None);
        assert!(entry_bounded.entries.is_empty());
        assert!(
            storage
                .get_item(COMPOSER_DRAFT_STORAGE_KEY)
                .unwrap()
                .is_none(),
            "an exact-size rejection must clear the stale persisted draft"
        );

        let mut total_bounded = PersistedComposerDraftStore::default();
        for index in 0..3 {
            assert_eq!(
                total_bounded.upsert(
                    owner(&format!("escaped-{index}")),
                    "\u{0001}".repeat(40 * 1024)
                ),
                DraftStoreUpdate::Stored { evicted: 0 }
            );
        }
        let escaped_newest = owner("escaped-2");
        let total_outcome = total_bounded.finalize_bounds(Some(&escaped_newest));
        assert_eq!(total_outcome.evicted, 1);
        assert!(!total_outcome.active_owner_too_large);
        assert!(total_bounded.find_index(&owner("escaped-0")).is_none());
        assert!(
            total_bounded
                .encoded_len()
                .is_ok_and(|bytes| bytes <= MAX_PERSISTED_COMPOSER_DRAFT_TOTAL_BYTES)
        );
        clear_storage();
    }

    #[wasm_bindgen_test]
    fn typing_path_queues_eviction_notice_until_persistence() {
        let state = AppState::new();
        let composer = state.composer_untracked();
        for index in 0..=MAX_PERSISTED_COMPOSER_DRAFT_ENTRIES {
            composer
                .draft_owner
                .set(Some(owner(&format!("typing-{index}"))));
            composer.text.set(format!("draft-{index}"));
            assert!(state.checkpoint_composer_draft(&composer));
        }
        assert_eq!(state.composer_draft_pending_evictions.get_untracked(), 1);
        assert!(
            !state.composer_draft_eviction_notified.get_untracked(),
            "typing may queue an eviction notice but must not open its modal"
        );
    }

    #[wasm_bindgen_test]
    fn draft_limit_notice_rearms_only_after_active_owner_is_persistable() {
        clear_storage();
        COMPOSER_DRAFT_LIMIT_NOTICES.with(|count| count.set(0));
        let state = AppState::new();
        let composer = state.composer_untracked();
        let active_owner = owner("sustained-oversize");
        composer.draft_owner.set(Some(active_owner.clone()));

        for extra in 0..3 {
            composer
                .text
                .set("x".repeat(MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES + extra));
            assert!(state.checkpoint_composer_draft(&composer));
            state.flush_composer_drafts();
        }
        COMPOSER_DRAFT_LIMIT_NOTICES.with(|count| {
            assert_eq!(
                count.get(),
                1,
                "sustained typing-path oversize must remain a single notice"
            );
        });
        assert!(state.composer_draft_limit_notified.get_untracked());

        composer.text.set("persistable".to_owned());
        assert!(state.checkpoint_composer_draft(&composer));
        state.flush_composer_drafts();
        assert!(!state.composer_draft_limit_notified.get_untracked());
        assert!(
            state
                .composer_drafts
                .with_untracked(|drafts| { drafts.find_index(&active_owner).is_some() })
        );

        let mut escape_expanded = "\u{0001}".repeat(50 * 1024);
        composer.text.set(escape_expanded.clone());
        assert!(state.checkpoint_composer_draft(&composer));
        state.flush_composer_drafts();
        escape_expanded.push('\u{0001}');
        composer.text.set(escape_expanded);
        assert!(state.checkpoint_composer_draft(&composer));
        state.flush_composer_drafts();
        COMPOSER_DRAFT_LIMIT_NOTICES.with(|count| {
            assert_eq!(
                count.get(),
                2,
                "escape-expanded oversize must also remain a single notice"
            );
        });
        assert!(state.composer_draft_limit_notified.get_untracked());

        composer.text.set("persistable again".to_owned());
        assert!(state.checkpoint_composer_draft(&composer));
        state.flush_composer_drafts();
        assert!(!state.composer_draft_limit_notified.get_untracked());
        composer
            .text
            .set("x".repeat(MAX_PERSISTED_COMPOSER_DRAFT_ENTRY_BYTES));
        assert!(state.checkpoint_composer_draft(&composer));
        COMPOSER_DRAFT_LIMIT_NOTICES.with(|count| {
            assert_eq!(
                count.get(),
                3,
                "a later oversize episode re-arms after a persisted active draft"
            );
        });
        clear_storage();
    }

    #[wasm_bindgen_test]
    fn injected_scheduler_debounces_writes_until_timer_or_flush() {
        clear_storage();
        let next_handle = Rc::new(Cell::new(0));
        let scheduled = Rc::new(RefCell::new(Vec::<js_sys::Function>::new()));
        let cancelled = Rc::new(RefCell::new(Vec::<i32>::new()));
        let scheduled_for_callback = scheduled.clone();
        let next_for_callback = next_handle.clone();
        let cancelled_for_callback = cancelled.clone();

        let state = AppState::new();
        let scheduler_id = state.composer_draft_persistence.id;
        COMPOSER_DRAFT_SCHEDULERS.with(|schedulers| {
            schedulers.borrow_mut().insert(
                scheduler_id,
                ComposerDraftScheduler {
                    schedule: Rc::new(move |callback, delay_ms| {
                        assert_eq!(delay_ms, COMPOSER_DRAFT_DEBOUNCE_MS);
                        let handle = next_for_callback.get() + 1;
                        next_for_callback.set(handle);
                        scheduled_for_callback.borrow_mut().push(callback.clone());
                        Ok(handle)
                    }),
                    cancel: Rc::new(move |handle| {
                        cancelled_for_callback.borrow_mut().push(handle);
                    }),
                },
            );
        });
        let composer = state.composer_untracked();
        composer.draft_owner.set(Some(owner("scheduled-host")));

        composer.text.set("f".to_owned());
        assert!(state.checkpoint_composer_draft(&composer));
        state.schedule_composer_draft_persist();
        composer.text.set("final".to_owned());
        assert!(state.checkpoint_composer_draft(&composer));
        state.schedule_composer_draft_persist();

        assert_eq!(&*cancelled.borrow(), &[1]);
        assert_eq!(scheduled.borrow().len(), 2);
        assert!(
            web_sys::window()
                .unwrap()
                .local_storage()
                .unwrap()
                .unwrap()
                .get_item(COMPOSER_DRAFT_STORAGE_KEY)
                .unwrap()
                .is_none(),
            "edits update memory but do not synchronously write localStorage"
        );

        scheduled.borrow()[1]
            .call0(&JsValue::UNDEFINED)
            .expect("the injected timer should invoke");
        let mut restored = load_composer_drafts();
        assert_eq!(
            restored.restore(&owner("scheduled-host")).as_deref(),
            Some("final")
        );

        composer.text.set("flushed".to_owned());
        state.flush_composer_drafts();
        let mut restored = load_composer_drafts();
        assert_eq!(
            restored.restore(&owner("scheduled-host")).as_deref(),
            Some("flushed"),
            "lifecycle flush bypasses the debounce deadline"
        );
        clear_storage();
        drop(state);
        COMPOSER_DRAFT_SCHEDULERS.with(|schedulers| {
            assert!(!schedulers.borrow().contains_key(&scheduler_id));
        });
        COMPOSER_DRAFT_TIMEOUTS.with(|timeouts| {
            assert!(!timeouts.borrow().contains_key(&scheduler_id));
        });
    }
}

/// A draft backend/launch-profile choice, scoped to the host it was made
/// against.
///
/// The host is part of the identity, not decoration: launch-profile ids are
/// only meaningful inside one host's catalog, and a backend enabled on host A
/// may be disabled on host B. An unscoped selection can be submitted to the
/// wrong host after a reload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedDraftSelection {
    pub host_id: String,
    #[serde(default)]
    pub backend: Option<BackendKind>,
    #[serde(default)]
    pub launch_profile: Option<LaunchProfileId>,
}

impl PersistedDraftSelection {
    fn is_empty(&self) -> bool {
        self.backend.is_none() && self.launch_profile.is_none()
    }
}

/// The user's explicit selection, as it must survive a reload.
///
/// Identifiers only. The center zone also holds open files, diffs, terminals
/// and scroll positions; those are caches and view state, not intent, and
/// persisting them would bloat storage and go stale. Everything here is a
/// pointer the app re-resolves against live server state on the way back in —
/// and drops when it no longer resolves.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedWorkspaceSelection {
    /// The chat that was open. Restored only if that exact identity is live.
    #[serde(default)]
    pub active_chat: Option<PersistedChatRef>,
    /// Draft selection for the next chat, scoped to its host.
    #[serde(default)]
    pub draft: Option<PersistedDraftSelection>,
}

impl PersistedWorkspaceSelection {
    /// Whether there is anything worth storing. Consulted only by the wasm
    /// writer, which removes the key rather than persisting a record saying
    /// the user chose nothing.
    #[cfg(target_arch = "wasm32")]
    fn is_empty(&self) -> bool {
        self.active_chat.is_none() && self.draft.is_none()
    }
}

#[cfg(target_arch = "wasm32")]
fn load_workspace_selection() -> PersistedWorkspaceSelection {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return PersistedWorkspaceSelection::default();
    };
    let Some(encoded) = storage
        .get_item(WORKSPACE_SELECTION_STORAGE_KEY)
        .ok()
        .flatten()
    else {
        return PersistedWorkspaceSelection::default();
    };
    match serde_json::from_str(&encoded) {
        Ok(selection) => selection,
        Err(error) => {
            log::warn!("invalid persisted workspace selection: {error}");
            let _ = storage.remove_item(WORKSPACE_SELECTION_STORAGE_KEY);
            PersistedWorkspaceSelection::default()
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_workspace_selection() -> PersistedWorkspaceSelection {
    PersistedWorkspaceSelection::default()
}

#[cfg(target_arch = "wasm32")]
fn persist_workspace_selection(selection: &PersistedWorkspaceSelection) {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return;
    };
    if selection.is_empty() {
        let _ = storage.remove_item(WORKSPACE_SELECTION_STORAGE_KEY);
        return;
    }
    match serde_json::to_string(selection) {
        Ok(encoded) => {
            if let Err(error) = storage.set_item(WORKSPACE_SELECTION_STORAGE_KEY, &encoded) {
                log::warn!("failed to persist workspace selection: {error:?}");
            }
        }
        Err(error) => log::warn!("failed to encode workspace selection: {error}"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_workspace_selection(_selection: &PersistedWorkspaceSelection) {}

// Test-only re-exports. The persistence functions are private so nothing but
// the effect writes them in production, but the reload lifecycle can only be
// covered from a browser test that seeds and reads real storage.
//
// Gated to wasm alongside the functions they wrap: their callers are the
// browser lifecycle tests in `dispatch`, which a native build does not compile,
// so an ungated `#[cfg(test)]` makes them look dead to every native lint.
#[cfg(all(test, target_arch = "wasm32"))]
pub fn persist_workspace_selection_for_tests(selection: &PersistedWorkspaceSelection) {
    persist_workspace_selection(selection);
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn load_workspace_selection_for_tests() -> PersistedWorkspaceSelection {
    load_workspace_selection()
}

#[cfg(all(test, target_arch = "wasm32"))]
pub fn persist_active_project_for_tests(project: Option<&ActiveProjectRef>) {
    persist_active_project(project);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabScrollState {
    pub scroll_top: i32,
    pub scroll_height: i32,
    pub client_height: i32,
    pub user_scrolled_up: bool,
}

pub fn next_tab_id() -> TabId {
    NEXT_TAB_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        TabId(id)
    })
}

/// A chat tab whose `agent_ref` has not yet been resolved because the user
/// opened a team member whose live binding does not exist yet. The first user
/// message sent in this tab is routed through `TeamMemberActivate` instead of
/// `SpawnAgent`, and the resulting `NewAgent` echo upgrades the tab's
/// `agent_ref` in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTeamMember {
    pub host_id: String,
    pub member_id: TeamMemberId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileResourceKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TabContent {
    Home,
    AgentMonitor,
    Chat {
        agent_ref: Option<ActiveAgentRef>,
        /// `Some` only while the user has opened a team member whose live
        /// agent hasn't been spawned yet. `None` for ordinary draft and live
        /// chat tabs — the discriminator that tells `submit_chat_input` to
        /// send `TeamMemberActivate` instead of `SpawnAgent::New`.
        pending_team_member: Option<PendingTeamMember>,
    },
    File {
        key: FileResourceKey,
    },
    Diff {
        /// Explicit owning project identity. Carried so a review overlay
        /// binds to the exact (host, project) the tab was opened for —
        /// resolving the project from `root` alone is ambiguous when two
        /// hosts/projects share the same root path string.
        host_id: String,
        project_id: ProjectId,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: String,
    },
    /// Compact review-comments surface for the project's single workspace
    /// draft review: snippets around each human comment, accepted AI comment,
    /// and pending AI suggestion — not the full diff — grouped by root. Binds
    /// to the explicit `(host_id, project_id)`; there is one active workspace
    /// review per project spanning every root.
    Comments {
        host_id: String,
        project_id: ProjectId,
    },
    /// Detail view for a Claude Code workflow run, opened from its tool
    /// card. Binds to the owning agent's chat plus the Workflow tool
    /// call id; live state is read from `AppState::workflow_runs`.
    Workflow {
        agent_ref: ActiveAgentRef,
        tool_call_id: ToolCallId,
    },
}

impl TabContent {
    pub fn empty_chat() -> Self {
        Self::Chat {
            agent_ref: None,
            pending_team_member: None,
        }
    }

    pub fn chat_with_agent(agent_ref: ActiveAgentRef) -> Self {
        Self::Chat {
            agent_ref: Some(agent_ref),
            pending_team_member: None,
        }
    }

    pub fn team_member_draft(host_id: String, member_id: TeamMemberId) -> Self {
        Self::Chat {
            agent_ref: None,
            pending_team_member: Some(PendingTeamMember { host_id, member_id }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Tab {
    pub id: TabId,
    pub content: TabContent,
    pub label: String,
    pub closeable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackingResource {
    File(FileResourceKey),
    Diff(DiffKey),
}

impl Tab {
    pub fn backing_resource(&self) -> Option<BackingResource> {
        match &self.content {
            TabContent::File { key } => Some(BackingResource::File(key.clone())),
            TabContent::Diff {
                host_id,
                project_id,
                root,
                scope,
                path,
            } => Some(BackingResource::Diff(DiffKey::new(
                host_id.clone(),
                project_id.clone(),
                root.clone(),
                *scope,
                path.clone(),
            ))),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneId {
    Primary,
    Secondary,
}

impl PaneId {
    pub fn other(self) -> Self {
        match self {
            Self::Primary => Self::Secondary,
            Self::Secondary => Self::Primary,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitRatio(f64);

impl SplitRatio {
    pub const MIN: f64 = 0.2;
    pub const MAX: f64 = 0.8;
    pub const DEFAULT: f64 = 0.5;

    pub fn new(value: f64) -> Self {
        if value.is_finite() {
            Self(value.clamp(Self::MIN, Self::MAX))
        } else {
            Self(Self::DEFAULT)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for SplitRatio {
    fn default() -> Self {
        Self::new(Self::DEFAULT)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Focused,
    // Only tests construct `Beside` since the open-to-side controls were
    // removed; `resolve` and the open-path entry points still honor it.
    #[allow(dead_code)]
    Beside,
}

pub const CENTER_TABS_DISABLED_REASON: &str = "Enable tabs to use split view.";
pub const TAB_SOURCE_MISSING_REASON: &str = "This tab is no longer open.";
pub const MOVE_ALREADY_IN_TARGET_PANE_REASON: &str = "This tab is already in that pane.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateFileEligibility {
    Enabled,
    TabsDisabled,
    SourceTabMissing,
    NotAFile,
    NotLoaded,
    TargetAlreadyContainsResource { existing: TabId },
}

impl DuplicateFileEligibility {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateFileResult {
    Duplicated {
        source: TabId,
        tab: TabId,
        target: PaneId,
    },
    ActivatedExisting {
        source: TabId,
        existing: TabId,
        target: PaneId,
    },
    TabsDisabled,
    SourceTabMissing,
    NotAFile,
    NotLoaded,
}

impl DuplicateFileResult {
    pub fn tab_id(self) -> Option<TabId> {
        match self {
            Self::Duplicated { tab, .. } => Some(tab),
            Self::ActivatedExisting { existing, .. } => Some(existing),
            Self::TabsDisabled | Self::SourceTabMissing | Self::NotAFile | Self::NotLoaded => None,
        }
    }
}

pub const MOVE_RESOURCE_ALREADY_IN_TARGET_REASON: &str =
    "This resource is already open in the other pane.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTabEligibility {
    Eligible,
    SourceTabMissing,
    AlreadyInTargetPane,
    ResourceAlreadyInTarget { existing: TabId },
}

impl MoveTabEligibility {
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Eligible => None,
            Self::SourceTabMissing => Some(TAB_SOURCE_MISSING_REASON),
            Self::AlreadyInTargetPane => Some(MOVE_ALREADY_IN_TARGET_PANE_REASON),
            Self::ResourceAlreadyInTarget { .. } => Some(MOVE_RESOURCE_ALREADY_IN_TARGET_REASON),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTabResult {
    Moved {
        tab: TabId,
        source: PaneId,
        target: PaneId,
    },
    SourceTabMissing,
    AlreadyInTargetPane,
    ResourceAlreadyInTarget {
        existing: TabId,
    },
}

impl MoveTabResult {
    pub fn disabled_reason(self) -> Option<&'static str> {
        match self {
            Self::Moved { .. } => None,
            Self::SourceTabMissing => Some(TAB_SOURCE_MISSING_REASON),
            Self::AlreadyInTargetPane => Some(MOVE_ALREADY_IN_TARGET_PANE_REASON),
            Self::ResourceAlreadyInTarget { .. } => Some(MOVE_RESOURCE_ALREADY_IN_TARGET_REASON),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingOpenDestination(PaneId);

impl PendingOpenDestination {
    pub fn new(pane: PaneId) -> Self {
        Self(pane)
    }

    pub fn pane(self) -> PaneId {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingFileNavigation {
    Line(u32),
    Offset(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingFileOpen {
    RefreshInPlace,
    Open {
        destination: PendingOpenDestination,
        navigation: Option<PendingFileNavigation>,
    },
}

#[derive(Clone, Debug)]
pub struct PaneState {
    pub tabs: Vec<Tab>,
    pub active_tab_id: Option<TabId>,
}

impl PaneState {
    fn empty() -> Self {
        Self {
            tabs: Vec::new(),
            active_tab_id: None,
        }
    }

    fn home() -> Self {
        let id = next_tab_id();
        Self {
            tabs: vec![Tab {
                id,
                content: TabContent::Home,
                label: "Home".to_string(),
                closeable: false,
            }],
            active_tab_id: Some(id),
        }
    }

    fn from_tab(tab: Tab) -> Self {
        let id = tab.id;
        Self {
            tabs: vec![tab],
            active_tab_id: Some(id),
        }
    }

    fn activate(&mut self, id: TabId) -> bool {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active_tab_id = Some(id);
            true
        } else {
            false
        }
    }

    fn remove_tabs(&mut self, doomed: &HashSet<TabId>) {
        let old_active = self.active_tab_id;
        let old_active_index =
            old_active.and_then(|id| self.tabs.iter().position(|tab| tab.id == id));
        self.tabs.retain(|tab| !doomed.contains(&tab.id));
        if old_active.is_none_or(|id| !self.tabs.iter().any(|tab| tab.id == id)) {
            self.active_tab_id = if self.tabs.is_empty() {
                None
            } else {
                Some(self.tabs[old_active_index.unwrap_or(0).min(self.tabs.len() - 1)].id)
            };
        }
    }
}

#[derive(Clone, Debug)]
pub enum CenterLayout {
    Single(PaneState),
    Split {
        primary: PaneState,
        secondary: PaneState,
        focused: PaneId,
        ratio: SplitRatio,
    },
}

#[derive(Clone, Debug)]
pub struct CenterZoneState {
    pub layout: CenterLayout,
}

impl CenterZoneState {
    pub fn new_home() -> Self {
        Self {
            layout: CenterLayout::Single(PaneState::home()),
        }
    }

    pub fn focused_id(&self) -> PaneId {
        match &self.layout {
            CenterLayout::Single(_) => PaneId::Primary,
            CenterLayout::Split { focused, .. } => *focused,
        }
    }

    pub fn is_split(&self) -> bool {
        matches!(self.layout, CenterLayout::Split { .. })
    }

    pub fn split_ratio(&self) -> Option<SplitRatio> {
        match &self.layout {
            CenterLayout::Single(_) => None,
            CenterLayout::Split { ratio, .. } => Some(*ratio),
        }
    }

    pub fn set_split_ratio(&mut self, value: SplitRatio) {
        if let CenterLayout::Split { ratio, .. } = &mut self.layout {
            *ratio = value;
        }
    }

    pub fn resolve(&self, target: OpenTarget) -> PaneId {
        match target {
            OpenTarget::Focused => self.focused_id(),
            OpenTarget::Beside => self.focused_id().other(),
        }
    }

    pub fn pane(&self, id: PaneId) -> Option<&PaneState> {
        match (&self.layout, id) {
            (CenterLayout::Single(primary), PaneId::Primary) => Some(primary),
            (CenterLayout::Single(_), PaneId::Secondary) => None,
            (CenterLayout::Split { primary, .. }, PaneId::Primary) => Some(primary),
            (CenterLayout::Split { secondary, .. }, PaneId::Secondary) => Some(secondary),
        }
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut PaneState> {
        match (&mut self.layout, id) {
            (CenterLayout::Single(primary), PaneId::Primary) => Some(primary),
            (CenterLayout::Single(_), PaneId::Secondary) => None,
            (CenterLayout::Split { primary, .. }, PaneId::Primary) => Some(primary),
            (CenterLayout::Split { secondary, .. }, PaneId::Secondary) => Some(secondary),
        }
    }

    pub fn focused_pane(&self) -> &PaneState {
        match &self.layout {
            CenterLayout::Single(primary) => primary,
            CenterLayout::Split {
                primary,
                secondary,
                focused,
                ..
            } => match focused {
                PaneId::Primary => primary,
                PaneId::Secondary => secondary,
            },
        }
    }

    pub fn panes(&self) -> impl Iterator<Item = (PaneId, &PaneState)> {
        [
            self.pane(PaneId::Primary)
                .map(|pane| (PaneId::Primary, pane)),
            self.pane(PaneId::Secondary)
                .map(|pane| (PaneId::Secondary, pane)),
        ]
        .into_iter()
        .flatten()
    }

    pub fn all_tabs(&self) -> impl Iterator<Item = (PaneId, &Tab)> {
        self.panes()
            .flat_map(|(pane_id, pane)| pane.tabs.iter().map(move |tab| (pane_id, tab)))
    }

    pub fn all_tab_ids(&self) -> Vec<TabId> {
        self.all_tabs().map(|(_, tab)| tab.id).collect()
    }

    #[cfg(all(test, target_arch = "wasm32"))]
    pub fn occurrences(&self, content: &TabContent) -> Vec<(PaneId, TabId)> {
        self.all_tabs()
            .filter(|(_, tab)| tab.content == *content)
            .map(|(pane, tab)| (pane, tab.id))
            .collect()
    }

    pub fn active_tab_id(&self) -> Option<TabId> {
        self.focused_pane().active_tab_id
    }

    pub fn pane_active_tab_id(&self, pane: PaneId) -> Option<TabId> {
        self.pane(pane).and_then(|pane| pane.active_tab_id)
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.all_tabs()
            .find_map(|(_, tab)| (tab.id == id).then_some(tab))
    }

    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        match &mut self.layout {
            CenterLayout::Single(primary) => primary.tabs.iter_mut().find(|tab| tab.id == id),
            CenterLayout::Split {
                primary, secondary, ..
            } => primary
                .tabs
                .iter_mut()
                .chain(secondary.tabs.iter_mut())
                .find(|tab| tab.id == id),
        }
    }

    pub fn for_each_tab_mut(&mut self, mut action: impl FnMut(PaneId, &mut Tab)) {
        match &mut self.layout {
            CenterLayout::Single(primary) => {
                for tab in &mut primary.tabs {
                    action(PaneId::Primary, tab);
                }
            }
            CenterLayout::Split {
                primary, secondary, ..
            } => {
                for tab in &mut primary.tabs {
                    action(PaneId::Primary, tab);
                }
                for tab in &mut secondary.tabs {
                    action(PaneId::Secondary, tab);
                }
            }
        }
    }

    pub fn find_tab_in(&self, pane: PaneId, content: &TabContent) -> Option<TabId> {
        self.pane(pane)?
            .tabs
            .iter()
            .find(|tab| tab.content == *content)
            .map(|tab| tab.id)
    }

    pub fn find_tab(&self, content: &TabContent) -> Option<TabId> {
        let focused = self.focused_id();
        self.find_tab_in(focused, content)
            .or_else(|| self.find_tab_in(focused.other(), content))
    }

    pub fn locate_tab(&self, id: TabId) -> Option<PaneId> {
        self.panes()
            .find_map(|(pane, state)| state.tabs.iter().any(|tab| tab.id == id).then_some(pane))
    }

    pub fn open(&mut self, content: TabContent, label: String, closeable: bool) -> TabId {
        if let Some(id) = self.find_tab(&content) {
            self.activate(id);
            return id;
        }
        let target = self.focused_id();
        self.open_in(target, content, label, closeable, SplitRatio::default())
    }

    pub fn open_in(
        &mut self,
        target: PaneId,
        content: TabContent,
        label: String,
        closeable: bool,
        ratio: SplitRatio,
    ) -> TabId {
        if let Some(id) = self.find_tab_in(target, &content) {
            self.activate(id);
            return id;
        }
        if !matches!(&content, TabContent::File { .. })
            && let Some(id) = self.find_tab(&content)
        {
            self.activate(id);
            return id;
        }
        let id = next_tab_id();
        let tab = Tab {
            id,
            content,
            label,
            closeable,
        };
        match (&mut self.layout, target) {
            (CenterLayout::Single(primary), PaneId::Primary) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
            }
            (CenterLayout::Single(_), PaneId::Secondary) => {
                let old_layout =
                    std::mem::replace(&mut self.layout, CenterLayout::Single(PaneState::empty()));
                let primary = match old_layout {
                    CenterLayout::Single(primary) => primary,
                    other => {
                        self.layout = other;
                        return id;
                    }
                };
                self.layout = CenterLayout::Split {
                    primary,
                    secondary: PaneState::from_tab(tab),
                    focused: PaneId::Secondary,
                    ratio,
                };
            }
            (
                CenterLayout::Split {
                    primary, focused, ..
                },
                PaneId::Primary,
            ) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
                *focused = PaneId::Primary;
            }
            (
                CenterLayout::Split {
                    secondary, focused, ..
                },
                PaneId::Secondary,
            ) => {
                secondary.tabs.push(tab);
                secondary.active_tab_id = Some(id);
                *focused = PaneId::Secondary;
            }
        }
        id
    }

    pub fn reveal_tab(&mut self, id: TabId) -> bool {
        let Some(pane_id) = self.locate_tab(id) else {
            return false;
        };
        if !self.set_active_tab_in_pane(pane_id, id) {
            return false;
        }
        if let CenterLayout::Split { focused, .. } = &mut self.layout {
            *focused = pane_id;
        }
        true
    }

    pub fn set_active_tab_in_pane(&mut self, pane: PaneId, id: TabId) -> bool {
        self.pane_mut(pane)
            .is_some_and(|pane_state| pane_state.activate(id))
    }

    pub fn activate(&mut self, id: TabId) {
        self.reveal_tab(id);
    }

    pub fn update_tab(&mut self, id: TabId, content: TabContent, label: String) -> bool {
        let Some(tab) = self.tab_mut(id) else {
            return false;
        };
        tab.content = content;
        tab.label = label;
        true
    }

    pub fn close(&mut self, id: TabId) {
        let Some(tab) = self.tab(id) else {
            return;
        };
        if !tab.closeable {
            return;
        }
        self.remove_tabs(&HashSet::from([id]));
    }

    pub fn replace_active(&mut self, content: TabContent, label: String, closeable: bool) -> TabId {
        if let Some(active_id) = self.active_tab_id()
            && let Some(tab) = self.tab_mut(active_id)
        {
            tab.content = content;
            tab.label = label;
            tab.closeable = closeable;
            return active_id;
        }
        self.open(content, label, closeable)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_tab_id().and_then(|id| self.tab(id))
    }

    pub fn active_content(&self) -> Option<&TabContent> {
        self.active_tab().map(|t| &t.content)
    }

    pub fn rename_tab_label(&mut self, id: TabId, new_label: String) {
        if let Some(tab) = self.tab_mut(id) {
            tab.label = new_label;
        }
    }

    pub fn composer_owner(&self) -> Option<(PaneId, TabId)> {
        let focused = self.focused_id();
        [focused, focused.other()].into_iter().find_map(|pane_id| {
            let pane = self.pane(pane_id)?;
            let tab_id = pane.active_tab_id?;
            let tab = pane.tabs.iter().find(|tab| tab.id == tab_id)?;
            matches!(&tab.content, TabContent::Chat { .. }).then_some((pane_id, tab_id))
        })
    }

    pub fn duplicate_file_to(
        &mut self,
        source: TabId,
        target: PaneId,
        ratio: SplitRatio,
    ) -> Option<TabId> {
        let tab = self.tab(source)?.clone();
        if !matches!(&tab.content, TabContent::File { .. }) {
            return None;
        }
        if let Some(existing) = self.find_tab_in(target, &tab.content) {
            self.activate(existing);
            return Some(existing);
        }
        Some(self.open_in(target, tab.content, tab.label, tab.closeable, ratio))
    }

    pub fn move_tab_eligibility(&self, target: PaneId, id: TabId) -> MoveTabEligibility {
        let Some(source) = self.locate_tab(id) else {
            return MoveTabEligibility::SourceTabMissing;
        };
        if source == target {
            return MoveTabEligibility::AlreadyInTargetPane;
        }
        let Some(tab) = self.tab(id) else {
            return MoveTabEligibility::SourceTabMissing;
        };
        if let Some(existing) = self.find_tab_in(target, &tab.content) {
            return MoveTabEligibility::ResourceAlreadyInTarget { existing };
        }
        MoveTabEligibility::Eligible
    }

    pub fn move_tab_to(&mut self, target: PaneId, id: TabId, ratio: SplitRatio) -> MoveTabResult {
        match self.move_tab_eligibility(target, id) {
            MoveTabEligibility::Eligible => {}
            MoveTabEligibility::SourceTabMissing => return MoveTabResult::SourceTabMissing,
            MoveTabEligibility::AlreadyInTargetPane => {
                return MoveTabResult::AlreadyInTargetPane;
            }
            MoveTabEligibility::ResourceAlreadyInTarget { existing } => {
                return MoveTabResult::ResourceAlreadyInTarget { existing };
            }
        }
        let Some(source) = self.locate_tab(id) else {
            return MoveTabResult::SourceTabMissing;
        };
        let Some(tab) = self.tab(id).cloned() else {
            return MoveTabResult::SourceTabMissing;
        };
        let mut doomed = HashSet::new();
        doomed.insert(id);
        if let Some(source_pane) = self.pane_mut(source) {
            source_pane.remove_tabs(&doomed);
        }
        match (&mut self.layout, target) {
            (CenterLayout::Single(primary), PaneId::Primary) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
            }
            (CenterLayout::Single(_), PaneId::Secondary) => {
                let old_layout =
                    std::mem::replace(&mut self.layout, CenterLayout::Single(PaneState::empty()));
                let primary = match old_layout {
                    CenterLayout::Single(primary) => primary,
                    other => {
                        self.layout = other;
                        return MoveTabResult::SourceTabMissing;
                    }
                };
                self.layout = CenterLayout::Split {
                    primary,
                    secondary: PaneState::from_tab(tab),
                    focused: PaneId::Secondary,
                    ratio,
                };
            }
            (
                CenterLayout::Split {
                    primary, focused, ..
                },
                PaneId::Primary,
            ) => {
                primary.tabs.push(tab);
                primary.active_tab_id = Some(id);
                *focused = PaneId::Primary;
            }
            (
                CenterLayout::Split {
                    secondary, focused, ..
                },
                PaneId::Secondary,
            ) => {
                secondary.tabs.push(tab);
                secondary.active_tab_id = Some(id);
                *focused = PaneId::Secondary;
            }
        }
        self.collapse_empty_pane();
        MoveTabResult::Moved {
            tab: id,
            source,
            target,
        }
    }

    pub fn split_tab_to(&mut self, target: PaneId, id: TabId, ratio: SplitRatio) -> MoveTabResult {
        let CenterLayout::Single(primary) = &self.layout else {
            return self.move_tab_to(target, id, ratio);
        };
        if !primary.tabs.iter().any(|tab| tab.id == id) {
            return MoveTabResult::SourceTabMissing;
        }

        let old_layout =
            std::mem::replace(&mut self.layout, CenterLayout::Single(PaneState::empty()));
        let CenterLayout::Single(mut remaining) = old_layout else {
            unreachable!("the layout was checked as single above");
        };
        let Some(tab) = remaining.tabs.iter().find(|tab| tab.id == id).cloned() else {
            self.layout = CenterLayout::Single(remaining);
            return MoveTabResult::SourceTabMissing;
        };
        let mut moved = HashSet::new();
        moved.insert(id);
        remaining.remove_tabs(&moved);

        let dragged = PaneState::from_tab(tab);
        let (primary, secondary) = match target {
            PaneId::Primary => (dragged, remaining),
            PaneId::Secondary => (remaining, dragged),
        };
        self.layout = CenterLayout::Split {
            primary,
            secondary,
            focused: target,
            ratio,
        };
        self.collapse_empty_pane();
        MoveTabResult::Moved {
            tab: id,
            source: PaneId::Primary,
            target,
        }
    }

    pub fn remove_tabs(&mut self, doomed: &HashSet<TabId>) {
        match &mut self.layout {
            CenterLayout::Single(primary) => primary.remove_tabs(doomed),
            CenterLayout::Split {
                primary, secondary, ..
            } => {
                primary.remove_tabs(doomed);
                secondary.remove_tabs(doomed);
            }
        }
        self.collapse_empty_pane();
    }

    fn collapse_empty_pane(&mut self) {
        let replacement = match &self.layout {
            CenterLayout::Single(primary) if primary.tabs.is_empty() => Some(PaneState::home()),
            CenterLayout::Split {
                primary, secondary, ..
            } if primary.tabs.is_empty() && secondary.tabs.is_empty() => Some(PaneState::home()),
            CenterLayout::Split {
                primary, secondary, ..
            } if primary.tabs.is_empty() => Some(secondary.clone()),
            CenterLayout::Split {
                primary, secondary, ..
            } if secondary.tabs.is_empty() => Some(primary.clone()),
            _ => None,
        };
        if let Some(pane) = replacement {
            self.layout = CenterLayout::Single(pane);
        }
    }
}

fn duplicate_file_eligibility_for(
    tabs_enabled: bool,
    center_zone: &CenterZoneState,
    open_files: &HashMap<FileResourceKey, OpenFile>,
    target: PaneId,
    source: TabId,
) -> DuplicateFileEligibility {
    if !tabs_enabled {
        return DuplicateFileEligibility::TabsDisabled;
    }
    let Some(tab) = center_zone.tab(source) else {
        return DuplicateFileEligibility::SourceTabMissing;
    };
    let TabContent::File { key } = &tab.content else {
        return DuplicateFileEligibility::NotAFile;
    };
    if !open_files.contains_key(key) {
        return DuplicateFileEligibility::NotLoaded;
    }
    if let Some(existing) = center_zone.find_tab_in(target, &tab.content) {
        return DuplicateFileEligibility::TargetAlreadyContainsResource { existing };
    }
    DuplicateFileEligibility::Enabled
}

impl Default for CenterZoneState {
    fn default() -> Self {
        Self::new_home()
    }
}

// ── Dock ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum DockVisibility {
    Visible,
    Hidden,
}

/// Which tab of the left dock is currently shown. Stored in `AppState` (rather
/// than locally in the dock component) so a keyboard shortcut and the
/// "search in folder" file-explorer action can switch to the Search tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftTab {
    Files,
    Git,
    Search,
    /// Find-references results panel (M5). Auto-activated when a Shift+F12
    /// find-references query runs.
    References,
}

/// Which tab of the right dock is currently shown. Stored in `AppState` so
/// global UI actions such as command-palette entries can open a specific panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightTab {
    Agents,
    Sessions,
    Teams,
    Workflows,
}

/// All persistent state for the project-wide search panel. Lives in `AppState`
/// so streamed results survive the panel being display-toggled (or its dock
/// being hidden) and so `dispatch` can append incoming result frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectSearchUiState {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub use_regex: bool,
    pub include_ignored: bool,
    /// When set, the search is scoped to this root-relative folder prefix
    /// (driven by the "search in folder" action).
    pub path_prefix: Option<String>,
    /// When non-empty, only these roots are searched (paired with
    /// `path_prefix` for "search in folder").
    pub roots: Vec<ProjectRootPath>,
    /// The `search_id` of the most recently issued search. Incoming result /
    /// complete frames are ignored unless they carry this id.
    pub active_search_id: u64,
    /// True between issuing a search and receiving its `complete` frame.
    pub in_flight: bool,
    /// One entry per matching file, in arrival order.
    pub results: Vec<ProjectSearchFileResult>,
    pub total_files: u32,
    pub total_matches: u32,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectReferencesMode {
    #[default]
    References,
    DefinitionTargets,
}

/// All persistent state for the find-references results panel (M5). Lives in
/// `AppState` so streamed results survive the panel being display-toggled and so
/// `dispatch` can append incoming `code_intel_references_results` frames. Mirrors
/// [`ProjectSearchUiState`], correlated by a `references_id` domain id and the
/// exact initiating occurrence/resource so late frames cannot be reconstructed
/// against whichever project or pane happens to be active at response time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectReferencesUiState {
    pub mode: ProjectReferencesMode,
    /// The exact file occurrence that initiated this query or definition
    /// chooser. Result routing can prefer this occurrence without consulting
    /// response-time focus; the resource/version remain the request identity.
    pub source_tab: Option<TabId>,
    pub source_key: Option<FileResourceKey>,
    pub source_version: Option<ProjectFileVersion>,
    /// The `references_id` of the most recently issued query. Incoming result /
    /// complete frames are ignored unless they carry this id and match the stored
    /// source resource context.
    pub active_references_id: u64,
    /// True between issuing a query and its terminal `complete` frame.
    pub in_flight: bool,
    /// The identifier the query is about, for the panel header. `None` when the
    /// symbol text wasn't captured.
    pub symbol: Option<String>,
    /// One entry per matching file, in arrival order.
    pub results: Vec<CodeIntelReferencesFileResult>,
    /// For `DefinitionTargets` mode, one target per rendered result row in
    /// flattened file/line order. References mode leaves this empty and rows
    /// navigate by line as before.
    pub row_targets: Vec<CodeIntelLocation>,
    pub total_files: u32,
    pub total_references: u32,
    pub truncated: bool,
    pub cancelled: bool,
    pub error: Option<String>,
}

impl ProjectReferencesUiState {
    pub fn source(&self) -> Option<(TabId, &FileResourceKey, ProjectFileVersion)> {
        Some((
            self.source_tab?,
            self.source_key.as_ref()?,
            self.source_version?,
        ))
    }
}

#[derive(Clone, Debug)]
pub struct ChatMessageEntry {
    pub message: ChatMessage,
    pub tool_requests: Vec<ToolRequestEntry>,
}

#[derive(Clone, Debug)]
pub struct ToolRequestEntry {
    pub tool_name: String,
    pub request: ToolRequest,
    pub result: Option<ToolExecutionCompletedData>,
}

#[derive(Clone, Debug)]
pub struct StreamingToolRequest {
    pub tool_call_id: String,
    pub entry: ArcRwSignal<ToolRequestEntry>,
}

// ── Chat transcript rows ────────────────────────────────────────────────

thread_local! {
    static NEXT_CHAT_ROW_ID: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChatRowId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub String);

fn next_chat_row_id() -> ChatRowId {
    NEXT_CHAT_ROW_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        ChatRowId(id)
    })
}

thread_local! {
    static NEXT_HISTORY_REQUEST_ID: Cell<u64> = const { Cell::new(0) };
    /// Per-process entropy, mixed into every request id so ids are opaque
    /// rather than a guessable sequence. Seeded lazily from the platform's
    /// randomness — `Math.random` in the browser, the clock natively — because
    /// the frontend has no RNG dependency and this is not a security boundary.
    static HISTORY_REQUEST_SALT: Cell<u64> = const { Cell::new(0) };
}

fn history_request_salt() -> u64 {
    HISTORY_REQUEST_SALT.with(|cell| {
        let existing = cell.get();
        if existing != 0 {
            return existing;
        }
        let seed = platform_entropy();
        // Never 0: that is the "unseeded" sentinel above.
        let seed = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        cell.set(seed);
        seed
    })
}

#[cfg(target_arch = "wasm32")]
fn platform_entropy() -> u64 {
    (js_sys::Math::random() * (u64::MAX as f64)) as u64
}

#[cfg(not(target_arch = "wasm32"))]
fn platform_entropy() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// An opaque, client-unique id for one `FetchSessionHistory`.
///
/// Compared only against the echo of this client's own request. It is opaque
/// rather than a bare counter so ids cannot be predicted or accidentally
/// collided across page loads, per the approved fresh-ID rule.
pub fn new_history_request_id() -> String {
    let salt = history_request_salt();
    NEXT_HISTORY_REQUEST_ID.with(|cell| {
        let seq = cell.get();
        cell.set(seq + 1);
        // SplitMix64 finalizer: cheap, dependency-free, and thoroughly mixes
        // the low-entropy counter into the salt.
        let mut z = salt.wrapping_add(seq.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        format!("{z:016x}")
    })
}

/// What a transcript row actually is.
///
/// The transcript used to be message-only, so every row could assume
/// `.entry.message`. Compaction introduces a row that is deliberately *not*
/// a message: it has no sender, no body, no copy control, and it must never
/// acquire one — a raw provider summary that picks up a `MessageSender` is
/// exactly the leak the compaction work exists to stop. Modelling it as a
/// second row kind (rather than a synthetic `MessageSender::System`) is what
/// makes that structural instead of a rendering convention.
///
/// The marker stays a *row* — not a card floated outside the windowed list —
/// so it inherits the virtualizer's `ResizeObserver` measurement and row
/// rhythm untouched, and so it survives history paging and scroll
/// restoration like any other row.
#[derive(Clone, Debug)]
pub enum ChatRowContent {
    Message(ArcRwSignal<ChatMessageEntry>),
    /// Signal-backed for the same reason messages are: the windowed list keys
    /// rows by `ChatRowId`, so a row whose content is refreshed in place —
    /// a later, richer sighting of the same marker — would otherwise keep
    /// rendering the value captured when it mounted.
    ContextCompaction(ArcRwSignal<ContextCompactionTimelineEvent>),
    /// A retry or cancellation notice, at the point in the conversation where
    /// it happened. See [`ChatNotice`] for why this is a row.
    Notice(ArcRwSignal<ChatNotice>),
}

#[derive(Clone, Debug)]
pub struct ChatRowHandle {
    pub id: ChatRowId,
    pub content: ChatRowContent,
}

impl ChatRowHandle {
    /// Build a message row. Named `new` so the existing call sites and test
    /// fixtures keep reading naturally; `message` is the explicit alias.
    pub fn new(entry: ChatMessageEntry) -> Self {
        Self::message(entry)
    }

    pub fn message(entry: ChatMessageEntry) -> Self {
        Self {
            id: next_chat_row_id(),
            content: ChatRowContent::Message(ArcRwSignal::new(entry)),
        }
    }

    pub fn context_compaction(event: ContextCompactionTimelineEvent) -> Self {
        Self {
            id: next_chat_row_id(),
            content: ChatRowContent::ContextCompaction(ArcRwSignal::new(event)),
        }
    }

    pub fn notice(notice: ChatNotice) -> Self {
        Self {
            id: next_chat_row_id(),
            content: ChatRowContent::Notice(ArcRwSignal::new(notice)),
        }
    }

    /// The row's message payload, or `None` for a non-message row.
    ///
    /// Every caller that walks the transcript looking for messages — tool-call
    /// attachment, metadata patching, context-breakdown lookup — goes through
    /// this and skips markers, rather than assuming every row has an entry.
    pub fn message_entry(&self) -> Option<&ArcRwSignal<ChatMessageEntry>> {
        match &self.content {
            ChatRowContent::Message(entry) => Some(entry),
            ChatRowContent::ContextCompaction(_) | ChatRowContent::Notice(_) => None,
        }
    }

    pub fn compaction_marker(&self) -> Option<&ArcRwSignal<ContextCompactionTimelineEvent>> {
        match &self.content {
            ChatRowContent::ContextCompaction(event) => Some(event),
            ChatRowContent::Message(_) | ChatRowContent::Notice(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHistoryState {
    pub message_count: u32,
    pub oldest_seq: Option<u64>,
    pub has_more_before: bool,
    /// The page request this client is currently waiting for, if any.
    ///
    /// This replaces a bare `loading: bool`. A boolean only says "a fetch is
    /// out"; it cannot say *which* fetch, so a page that arrives after the
    /// client has moved on — the reconnect case, where bootstrap wipes the
    /// transcript while a fetch from the previous connection is still in
    /// flight — was prepended into a transcript it does not belong to.
    /// Correlating on the echoed request id and cursor makes a stale page
    /// droppable instead of duplicating rows.
    pub pending_request: Option<PendingHistoryRequest>,
}

impl SessionHistoryState {
    pub fn loading(&self) -> bool {
        self.pending_request.is_some()
    }
}

/// The outstanding `FetchSessionHistory` request: its id and the cursor it
/// asked for. A `SessionHistory` frame applies only when both match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHistoryRequest {
    pub request_id: HistoryPageRequestId,
    pub before_seq: Option<u64>,
}

// ── Context compaction ──────────────────────────────────────────────────
//
// Three records, deliberately separate, because they have three lifetimes:
//
//   * `context_compactions`  — the transient operation. Lives from request to
//     terminal, drives the banner and the busy pill, is reconstructed from
//     bootstrap, and is *not* a transcript row.
//   * `context_compaction_rows` — the durable marker index. One entry per
//     marker id, permanent, so the same compaction arriving from live,
//     bootstrap, a paged history window, and a legacy provider import
//     materializes exactly one row.
//   * `compaction_capability` — whether a request is even offerable, so the
//     controls can be visible-but-disabled with a reason instead of silently
//     vanishing.
//
// None of these is the old replacement machinery (`compaction_in_progress` and
// friends), which stays untouched below for legacy replacement lineage.

/// Where the client is in a compaction request, from the client's point of
/// view.
///
/// `Requesting` exists only locally and only between the click and the first
/// server frame. Without it, two controls (header button and agent card) can
/// both submit before either sees an operation id, and the server admits two
/// requests for one user intent.
///
/// Every variant here describes work that is *outstanding*, which is what
/// earns the banner its place at the tip of the transcript. There is
/// deliberately no terminal variant: a finished compaction — succeeded or
/// failed — is recorded by the durable marker row at the point in the
/// conversation where it happened, and its error text by `compaction_errors`
/// for the agent card. A retained terminal banner was a third copy that,
/// being rendered outside the windowed list, stayed welded to the end of the
/// conversation while new turns appeared above it.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextCompactionUiState {
    /// Sent, no server frame yet. No operation id exists.
    Requesting,
    /// The server owns an operation. `payload.status` carries the detail.
    ///
    /// `live` is false for a banner reconstructed from `AgentBootstrap`. A
    /// reconstructed banner must be *visible* but must not announce: inserting
    /// a node into an `aria-live` region is itself an announcement, so
    /// suppressing the explicit announce call is not enough to make a
    /// reconnect silent. The first genuinely live update flips it.
    ///
    /// Boxed because the payload dwarfs `Requesting`, and every holder of this
    /// enum — the per-agent map, and the `Memo` the banner reads — would
    /// otherwise pay the full payload size for the common empty case.
    Active {
        payload: Box<ContextCompactionNotifyPayload>,
        live: bool,
    },
}

impl ContextCompactionUiState {
    /// Is work outstanding? Drives the busy pill, the derived agent state, and
    /// the duplicate-submit gate.
    pub fn is_in_flight(&self) -> bool {
        match self {
            Self::Requesting => true,
            Self::Active { payload, .. } => !payload.status.is_terminal(),
        }
    }

    /// Deferred means "the server has it and is waiting for a safe point" —
    /// a distinct, user-meaningful state from actively compacting, and the one
    /// most likely to be mistaken for a hang.
    pub fn is_deferred(&self) -> bool {
        matches!(
            self,
            Self::Active { payload, .. }
                if matches!(payload.status, ContextCompactionStatus::Deferred { .. })
        )
    }

    pub fn operation_id(&self) -> Option<&CompactionOperationId> {
        match self {
            Self::Requesting => None,
            Self::Active { payload, .. } => Some(&payload.operation_id),
        }
    }

    /// May this banner's live region announce? False for a bootstrap-restored
    /// banner until a live frame updates it.
    pub fn announces(&self) -> bool {
        match self {
            Self::Requesting => true,
            Self::Active { live, .. } => *live,
        }
    }
}

/// Fold a second sighting of the same marker into the one already rendered.
///
/// The same compaction reaches this client from live, bootstrap, a paged
/// window, and a one-time provider import, and those copies are not equally
/// rich — a live marker can race past metrics a later replay carries. The rule
/// is monotone accumulation: a field the existing copy already knows is never
/// unlearned, and a field it is missing is filled in. That makes the result
/// independent of arrival order, which matters because the order genuinely
/// varies between reconnect and page-back.
///
/// Identity fields (`marker_id`, `trigger`, `method`, `status`, `mutation`)
/// describe one completed event and are expected to agree; the existing copy
/// keeps them.
pub fn merge_richer_marker(
    existing: &mut ContextCompactionTimelineEvent,
    incoming: ContextCompactionTimelineEvent,
) {
    let metrics = &mut existing.metrics;
    let incoming_metrics = incoming.metrics;
    metrics.before_tokens = metrics.before_tokens.or(incoming_metrics.before_tokens);
    metrics.after_tokens = metrics.after_tokens.or(incoming_metrics.after_tokens);
    metrics.before_messages = metrics.before_messages.or(incoming_metrics.before_messages);
    metrics.after_messages = metrics.after_messages.or(incoming_metrics.after_messages);
    metrics.messages_summarized = metrics
        .messages_summarized
        .or(incoming_metrics.messages_summarized);
    metrics.cumulative_dropped_tokens = metrics
        .cumulative_dropped_tokens
        .or(incoming_metrics.cumulative_dropped_tokens);
    metrics.duration_ms = metrics.duration_ms.or(incoming_metrics.duration_ms);
    metrics.precomputed = metrics.precomputed.or(incoming_metrics.precomputed);

    existing.operation_id = existing.operation_id.take().or(incoming.operation_id);
    existing.provider_session_id = existing
        .provider_session_id
        .take()
        .or(incoming.provider_session_id);
    existing.message = existing.message.take().or(incoming.message);
    if existing.timestamp == 0 {
        existing.timestamp = incoming.timestamp;
    }
}

/// A capability snapshot, kept with the logical session it describes.
///
/// Capability is a property of a *logical session*, not of an agent id. Without
/// the session, a snapshot from a previous session silently authorizes (or
/// forbids) compaction in the current one.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionCapabilitySnapshot {
    pub logical_session_id: SessionId,
    pub availability: RequestedCompactionAvailability,
}

/// How many terminal operation ids to remember per agent.
///
/// A late `Progress` frame for an operation that already finished must not
/// resurrect the banner. Remembering the terminal ids is the only way to tell
/// "stale frame for a finished operation" from "frame for an operation this
/// client has not seen yet" (which happens legitimately after a reconnect).
/// The set is bounded because it is unbounded otherwise: a long-lived agent
/// compacts many times.
const TERMINAL_COMPACTION_MEMORY: usize = 16;

/// Bounded FIFO of operation ids known to have reached a terminal state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalOperationIds {
    order: VecDeque<CompactionOperationId>,
    seen: HashSet<CompactionOperationId>,
}

impl TerminalOperationIds {
    pub fn insert(&mut self, id: CompactionOperationId) {
        if self.seen.contains(&id) {
            return;
        }
        self.seen.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > TERMINAL_COMPACTION_MEMORY {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
    }

    pub fn contains(&self, id: &CompactionOperationId) -> bool {
        self.seen.contains(id)
    }
}

#[derive(Clone, Debug)]
pub struct OpenFile {
    pub path: ProjectPath,
    /// Version of these contents, from the project-stream actor's centralized
    /// counter. Code-intel frames apply only when their version equals this.
    pub version: ProjectFileVersion,
    pub contents: Option<String>,
    pub is_binary: bool,
    /// Server-reported: the file no longer exists on disk (a refresh read
    /// answered `missing`). `contents` keeps the last-seen text so the viewer
    /// can label it "deleted" instead of going blank; cleared by the next
    /// contents frame after the file is re-created.
    pub missing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileFocus {
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
}

/// Key for the code-intelligence signal. Carries the explicit owning
/// `(host_id, project_id)` plus the file path, so two projects/hosts that share
/// the same root-path string can't collide. The `ProjectFileVersion` is tracked
/// *inside* [`CodeIntelFileState`] (the version-equals-rendered rule), not in
/// the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CodeIntelKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}

impl From<&FileResourceKey> for CodeIntelKey {
    fn from(key: &FileResourceKey) -> Self {
        Self {
            host_id: key.host_id.clone(),
            project_id: key.project_id.clone(),
            path: key.path.clone(),
        }
    }
}

/// The semantic data the server pushed for one file version.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodeIntelData {
    pub status: Option<CodeIntelStatusPayload>,
    pub model: Option<CodeIntelFileModelPayload>,
    pub error: Option<CodeIntelErrorPayload>,
    /// Latest full-file diagnostics snapshot for this version. A
    /// `code_intel_diagnostics` frame **replaces** this set wholesale (spec
    /// §4.2) — diagnostics are not merged like the definition model.
    pub diagnostics: Vec<CodeIntelDiagnostic>,
}

impl CodeIntelData {
    /// Merge an incoming file model into the existing one for the same `(path,
    /// version)`. The server delivers the whole-file model **incrementally**
    /// (spec §2.1/§4.2): the first frame carries occurrence ranges, later frames
    /// at the same version fill in `definition` targets per occurrence. So
    /// occurrences are merged **by range**, and within a matching range the
    /// `definition` targets are **unioned** (deduped) rather than overwritten —
    /// a later frame that re-sends a range with an empty/partial `definition`
    /// must never wipe a target that an earlier frame already resolved. This is
    /// what makes the streamed go-to-definition map (M3) converge instead of
    /// flapping. The latest frame's `completeness` / `model_range` / `provider`
    /// / `language` win; `role` takes the latest, `display` the latest non-empty.
    pub fn merge_model(&mut self, incoming: CodeIntelFileModelPayload) {
        match self.model.as_mut() {
            None => self.model = Some(incoming),
            Some(existing) => {
                for occurrence in incoming.occurrences {
                    match existing
                        .occurrences
                        .iter_mut()
                        .find(|candidate| candidate.range == occurrence.range)
                    {
                        Some(slot) => merge_occurrence(slot, occurrence),
                        None => existing.occurrences.push(occurrence),
                    }
                }
                existing.completeness = incoming.completeness;
                existing.model_range = incoming.model_range;
                existing.provider = incoming.provider;
                existing.language = incoming.language;
                existing.version = incoming.version;
            }
        }
    }
}

/// Merge an incoming occurrence into an existing one with the same range.
/// `definition` targets are unioned (deduped) so already-resolved targets
/// survive a later frame that re-sends the range with an empty/partial set;
/// `role` takes the latest value and `display` the latest non-empty value.
fn merge_occurrence(slot: &mut CodeIntelOccurrence, incoming: CodeIntelOccurrence) {
    for location in incoming.definition {
        if !slot.definition.contains(&location) {
            slot.definition.push(location);
        }
    }
    slot.role = incoming.role;
    if !incoming.display.is_empty() {
        slot.display = incoming.display;
    }
}

/// Per-file code-intelligence state, implementing the version-equals-rendered
/// rule (`dev-docs/24-code-intelligence.md` §6): a frame is *applied* only when
/// its version equals the version of the file contents currently rendered; a
/// *newer* frame is *stashed* until the matching contents arrive; an *older*
/// frame is *dropped*.
///
/// The data is held in `by_version` (the "keyed by version" dimension); the
/// applied data is `by_version[rendered_version]`. This unifies apply and stash
/// into a single insert and makes both stale-drop directions fall out of the
/// `rendered_version` bookkeeping.
const CODE_INTEL_PRE_CONTENT_STASH_LIMIT: usize = 8;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodeIntelFileState {
    /// Version of the file contents currently rendered (from
    /// `ProjectFileContents`). `None` until the first contents arrive.
    pub rendered_version: Option<ProjectFileVersion>,
    pub by_version: std::collections::BTreeMap<ProjectFileVersion, CodeIntelData>,
}

impl CodeIntelFileState {
    /// Merge a versioned code-intel frame, honoring apply / stash / drop.
    /// `apply` mutates the [`CodeIntelData`] for that version. A frame older
    /// than the rendered version is dropped (it would paint over newer text).
    pub fn merge_versioned(
        &mut self,
        version: ProjectFileVersion,
        apply: impl FnOnce(&mut CodeIntelData),
    ) {
        if let Some(rendered) = self.rendered_version
            && version < rendered
        {
            // Older than what's on screen: drop.
            return;
        }
        // Equal (apply) or newer (stash): both merge into `by_version`.
        apply(self.by_version.entry(version).or_default());
        if self.rendered_version.is_none() {
            while self.by_version.len() > CODE_INTEL_PRE_CONTENT_STASH_LIMIT {
                self.by_version.pop_first();
            }
        }
    }

    /// Record that file contents at `version` are now rendered. Drops any
    /// stashed data older than `version` (it can never be shown again), which
    /// promotes the matching-version data to "applied".
    pub fn set_rendered_version(&mut self, version: ProjectFileVersion) {
        self.rendered_version = Some(version);
        self.by_version.retain(|candidate, _| *candidate >= version);
    }

    /// The data to render right now: the entry matching the rendered version,
    /// or `None` if contents haven't arrived or no frame matches yet.
    pub fn applied(&self) -> Option<&CodeIntelData> {
        self.by_version.get(&self.rendered_version?)
    }

    pub fn resolved_definition_at(
        &self,
        version: ProjectFileVersion,
        offset: u32,
    ) -> Option<(ByteRange, CodeIntelLocation)> {
        if self.rendered_version != Some(version) {
            return None;
        }
        let model = self.applied()?.model.as_ref()?;
        let occurrence = model
            .occurrences
            .iter()
            .find(|occ| occ.range.start <= offset && offset < occ.range.end)?;
        let location = occurrence.definition.first()?.clone();
        Some((occurrence.range, location))
    }

    pub fn navigable_range_at(
        &self,
        version: ProjectFileVersion,
        offset: u32,
    ) -> Option<ByteRange> {
        self.resolved_definition_at(version, offset)
            .map(|(range, _)| range)
    }

    /// The diagnostics whose range contains `offset`, at the rendered version.
    /// Used to merge diagnostic messages into the hover popover: the squiggle
    /// itself carries no readable message, so hovering the flagged span is the
    /// one place the user can read what is wrong. Zero-width ranges (some
    /// servers anchor a diagnostic to a single position) match their anchor
    /// offset. Order: most severe first, so the error reads before its hints.
    pub fn diagnostics_at(
        &self,
        version: ProjectFileVersion,
        offset: u32,
    ) -> Vec<CodeIntelDiagnostic> {
        if self.rendered_version != Some(version) {
            return Vec::new();
        }
        let Some(data) = self.applied() else {
            return Vec::new();
        };
        let mut hits: Vec<CodeIntelDiagnostic> = data
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                let range = diagnostic.range;
                (range.start <= offset && offset < range.end)
                    || (range.start == range.end && offset == range.start)
            })
            .cloned()
            .collect();
        hits.sort_by_key(|diagnostic| severity_sort_rank(diagnostic.severity));
        hits
    }
}

/// Ascending sort rank: most severe first.
fn severity_sort_rank(severity: protocol::CodeIntelSeverity) -> u8 {
    match severity {
        protocol::CodeIntelSeverity::Error => 0,
        protocol::CodeIntelSeverity::Warning => 1,
        protocol::CodeIntelSeverity::Information => 2,
        protocol::CodeIntelSeverity::Hint => 3,
    }
}

/// Context for the most recent on-demand go-to-definition request (M2), stored
/// when the `code_intel_navigate` frame is sent. A `code_intel_navigate_result`
/// is only acted on when it still matches this whole context — same
/// `navigate_id`, same owning host/project, and the source file still open at
/// the same rendered version — so a result that arrives after the tab closed,
/// the file changed, or the user switched projects is dropped instead of
/// yanking the user somewhere unexpected.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeIntelNavigateContext {
    pub navigate_id: u64,
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
}

/// On-demand hover popover state (M2). The anchor is captured (in viewport
/// coordinates) when the hover request fires, so the popover can be positioned
/// over the hovered span the moment the correlated `code_intel_hover_result`
/// arrives. `contents` is `None` while the request is in flight — the popover
/// renders nothing until real markdown lands (no empty flash).
#[derive(Clone, Debug, PartialEq)]
pub struct HoverPopover {
    pub hover_id: u64,
    /// The exact occurrence that owns the anchor and request. A late response
    /// cannot attach to another pane's occurrence of the same file.
    pub tab: TabId,
    pub key: FileResourceKey,
    pub version: ProjectFileVersion,
    /// Absolute file byte offset the hover targets. Used to dedupe rapid
    /// mousemoves over the same identifier so the popover doesn't flicker.
    pub offset: u32,
    /// Left edge of the hovered span, viewport-relative px.
    pub anchor_left: f64,
    /// Top edge of the hovered span, viewport-relative px.
    pub anchor_top: f64,
    /// Bottom edge of the hovered span, viewport-relative px.
    pub anchor_bottom: f64,
    /// Rendered markdown, or `None` until the result arrives.
    pub contents: Option<String>,
}

/// A transient, per-tab code-intelligence notice (e.g. "definition is outside
/// the project"). Rendered as a small banner by the owning file view, which
/// also owns the auto-clear timeout. Distinct from `CodeIntelData.error`:
/// notices are informational one-shots, never a provider failure state.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeIntelNotice {
    pub tab: TabId,
    pub message: String,
}

/// Cache key for `diff_contents`. Carries the explicit owning `(host_id,
/// project_id)` in addition to `(root, scope, path)` so two projects/hosts
/// that share the same root path string can't overwrite each other's diff —
/// the rendered diff body always belongs to the tab's project. `path` is the
/// file path, or empty for the whole-root (all-uncommitted) review surface.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffKey {
    pub host_id: String,
    pub project_id: ProjectId,
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: String,
}

impl DiffKey {
    pub fn new(
        host_id: impl Into<String>,
        project_id: ProjectId,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: impl Into<String>,
    ) -> Self {
        Self {
            host_id: host_id.into(),
            project_id,
            root,
            scope,
            path: path.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiffViewState {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    /// The context mode of the most recent *request* (not response). The
    /// reactive re-request effect compares this to `AppState::diff_context_mode`
    /// to decide whether to dispatch a new read, and the dispatch reducer
    /// compares `payload.context_mode` to this to reject stale responses.
    pub context_mode: DiffContextMode,
    /// True between the time a `ProjectReadDiff` is dispatched and a matching
    /// response arrives. The renderer shows a loading state when `pending` is
    /// set so stale data doesn't sit on screen while a fresh request is in
    /// flight.
    pub pending: bool,
    pub files: Vec<ProjectGitDiffFile>,
}

impl DiffViewState {
    /// Build the state to store when dispatching a fresh `ProjectReadDiff`.
    /// If the previous entry is for the same `context_mode`, its `files` are
    /// preserved to avoid flicker while refreshing. On a mode change, `files`
    /// is cleared so stale data is not visible.
    pub fn for_request(
        previous: Option<&DiffViewState>,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
        context_mode: DiffContextMode,
    ) -> DiffViewState {
        let files = previous
            .filter(|p| p.context_mode == context_mode)
            .map(|p| p.files.clone())
            .unwrap_or_default();
        DiffViewState {
            root,
            scope,
            path,
            context_mode,
            pending: true,
            files,
        }
    }
}

/// Pure reducer for `ProjectGitDiff` responses. Returns `Some(new_state)` if
/// the payload should replace the stored entry, or `None` if it should be
/// ignored as stale.
///
/// A response is considered valid only when a matching request is still the
/// latest one in flight — i.e. when `current.context_mode ==
/// payload.context_mode`. If no entry exists (response without an outstanding
/// request), the payload is ignored.
pub fn reduce_diff_response(
    current: Option<&DiffViewState>,
    payload: ProjectGitDiffPayload,
) -> Option<DiffViewState> {
    let current = current?;
    if current.context_mode != payload.context_mode {
        return None;
    }
    Some(DiffViewState {
        root: payload.root,
        scope: payload.scope,
        path: payload.path,
        context_mode: payload.context_mode,
        pending: false,
        files: payload.files,
    })
}

#[derive(Clone, Debug)]
pub struct StreamingState {
    pub agent_name: String,
    pub model: Option<String>,
    pub text: ArcRwSignal<String>,
    pub reasoning: ArcRwSignal<String>,
    pub tool_requests: ArcRwSignal<Vec<StreamingToolRequest>>,
}

#[derive(Clone, Debug)]
pub struct TerminalInfo {
    pub host_id: String,
    pub terminal_id: TerminalId,
    pub stream: StreamPath,
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: u64,
    /// Output chunks that arrived before the xterm widget mounted. Drained by
    /// the terminal view on first mount; not used afterwards.
    pub pending_output: Vec<String>,
    /// True once an xterm instance has been created for this terminal. Output
    /// is written directly through the JS bridge from then on.
    pub widget_mounted: bool,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<String>,
}

/// Something that happened to the *turn* rather than in it: the provider
/// backed off and retried, or the turn was cancelled.
///
/// These are transcript rows, not floating cards. They were floating cards,
/// rendered after the windowed list and cleared wholesale at the next turn —
/// which meant a retry from one point in the conversation rendered below every
/// message that arrived after it, and repeated attempts stacked. They arrive
/// in the server's ordered event stream like any other chat event, so the
/// honest projection is a row at the point they occurred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatNotice {
    OperationCancelled {
        message: String,
    },
    RetryAttempt {
        attempt: u64,
        max_retries: u64,
        error: String,
        backoff_ms: u64,
    },
}

/// One entry in an agent's Tycode orchestration log, in chronological order.
/// The log stores the typed events the server emitted verbatim, plus a
/// locally-injected [`OrchestrationRecord::Cancelled`] marker for turn
/// cancellations or fatal agent failure. The orchestration panel folds this log
/// into a presentation tree at render time (see `components::orchestration_view`)
/// — no aggregated state is cached; the events are the source of truth.
#[derive(Clone, Debug)]
pub enum OrchestrationRecord {
    Event(protocol::OrchestrationEvent),
    /// A `ChatEvent::OperationCancelled` or fatal agent failure at this point in
    /// the stream. Tycode drops any in-flight fan-out/worker/sub-agent without
    /// terminal events on either path, so the fold closes everything still
    /// running at this marker instead of leaving it stuck "running".
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectInfo {
    pub host_id: String,
    pub project: Project,
}

pub fn root_display_name(root: &ProjectRootPath) -> String {
    display_path_name(&root.0)
}

pub fn display_path_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .trim_end_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
        .to_owned()
}

pub fn sort_project_infos(projects: &mut [ProjectInfo]) {
    projects.sort_by(|left, right| {
        left.host_id
            .cmp(&right.host_id)
            .then(left.project.sort_order.cmp(&right.project.sort_order))
            .then(left.project.name.cmp(&right.project.name))
            .then(left.project.id.0.cmp(&right.project.id.0))
    });
}

/// Close every Chat tab in `center_zone` whose `agent_ref` points at
/// `(host_id, agent_id)`. Mirror of `dispatch::close_agent_tabs`, kept
/// in `state` so `finalize_compaction_close` can reuse it without
/// pulling state internals into the dispatcher.
fn close_agent_tabs_in_cz(
    center_zone: &mut CenterZoneState,
    host_id: &str,
    agent_id: &AgentId,
) -> HashSet<TabId> {
    let remove_ids: Vec<_> = center_zone
        .all_tabs()
        .filter(|(_, tab)| {
            matches!(
                &tab.content,
                TabContent::Chat { agent_ref: Some(ar), .. }
                    if ar.host_id == host_id && ar.agent_id == *agent_id
            )
        })
        .map(|(_, tab)| tab.id)
        .collect();
    let mut removed = HashSet::new();
    for id in remove_ids {
        center_zone.close(id);
        if center_zone.tab(id).is_none() {
            removed.insert(id);
        }
    }
    removed
}

fn close_host_runtime_tabs_in_cz(
    center_zone: &mut CenterZoneState,
    host_id: &str,
) -> HashSet<TabId> {
    let remove_ids: Vec<_> = center_zone
        .all_tabs()
        .filter(|(_, tab)| match &tab.content {
            TabContent::Chat {
                agent_ref,
                pending_team_member,
            } => {
                agent_ref
                    .as_ref()
                    .is_some_and(|agent_ref| agent_ref.host_id == host_id)
                    || pending_team_member
                        .as_ref()
                        .is_some_and(|pending| pending.host_id == host_id)
            }
            TabContent::Diff {
                host_id: tab_host, ..
            }
            | TabContent::Comments {
                host_id: tab_host, ..
            } => tab_host == host_id,
            TabContent::Workflow { agent_ref, .. } => agent_ref.host_id == host_id,
            TabContent::Home | TabContent::AgentMonitor | TabContent::File { .. } => false,
        })
        .map(|(_, tab)| tab.id)
        .collect();
    let mut removed = HashSet::new();
    for id in remove_ids {
        center_zone.close(id);
        if center_zone.tab(id).is_none() {
            removed.insert(id);
        }
    }
    removed
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub host_id: String,
    pub summary: SessionSummary,
}

/// What a `BrowseDialog` is opening for. Lets the same browser component serve
/// different consumers (project create, future: add-root, pick-file, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowsePurpose {
    OpenProject,
    AddRoot { project_id: ProjectId },
}

#[derive(Clone, Debug)]
pub struct BrowseDialogState {
    pub host_id: ArcRwSignal<String>,
    pub browse_stream: ArcRwSignal<StreamPath>,
    pub purpose: BrowsePurpose,
    pub include_hidden: ArcRwSignal<bool>,
    /// Set once `HostBrowseOpened` arrives.
    pub platform: ArcRwSignal<Option<HostPlatform>>,
    pub separator: ArcRwSignal<char>,
    pub home: ArcRwSignal<Option<HostAbsPath>>,
    pub current_path: ArcRwSignal<Option<HostAbsPath>>,
    pub parent: ArcRwSignal<Option<HostAbsPath>>,
    pub entries: ArcRwSignal<Vec<HostBrowseEntry>>,
    pub error: ArcRwSignal<Option<HostBrowseErrorPayload>>,
    pub loading: ArcRwSignal<bool>,
}

/// Snapshot of center-zone UI state for a single project. Persisted while the
/// user browses around so that flipping back to a project restores exactly the
/// view they left — and opening a different project does not leak state from
/// another.
#[derive(Clone, Debug, Default)]
pub struct ProjectViewMemory {
    pub center_zone: Option<CenterZoneState>,
    pub active_terminal: Option<ActiveTerminalRef>,
    pub open_files: HashMap<FileResourceKey, OpenFile>,
    pub diff_contents: HashMap<DiffKey, DiffViewState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ActiveProjectRef {
    pub host_id: String,
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingAgentSessionSettings {
    id: u64,
    values: SessionSettingsValues,
}

type PendingAgentSessionSettingsByProject =
    HashMap<(String, Option<ProjectId>), VecDeque<PendingAgentSessionSettings>>;

/// Latest server-emitted Add-report shuffle suggestion plus a monotonic
/// `serial` so the open dialog can apply only fresh suggestions and
/// ignore stale ones still sitting in state on re-open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamMemberShuffleSuggestionEntry {
    pub suggestion: TeamMemberShuffleSuggestion,
    pub serial: u64,
}

/// Per-project filter state for the Agents panel. Stored per active project
/// (keyed by `Option<ActiveProjectRef>`, where `None` represents the Home
/// project) so user toggles persist across project switches for the life of
/// the app.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentsPanelFilters {
    pub hide_sub_agents: bool,
    pub hide_inactive: bool,
    pub show_other_projects: bool,
}

/// Short-lived, non-persisted optimistic overlay for in-flight Agents-view
/// preference mutations. Each field is `Some` only while a change to that
/// preference domain has been sent to the server and the confirming
/// `AgentsViewPreferencesNotify` (or a fresh bootstrap snapshot) has not yet
/// arrived. The overlay is layered on top of the server snapshot so the UI
/// reacts instantly, but it is never written to disk and can never become a
/// durable second source of truth — which is precisely what kept the Agents
/// tab from flickering before this design.
///
/// Reconciliation is **drop-on-any-authoritative-snapshot**: an
/// `AgentsViewPreferencesNotify` (or a primary-host bootstrap) is a *full*
/// snapshot, so once one arrives the whole overlay is discarded — the server
/// value wins even when it differs from the optimistic one (the server
/// canonicalizes filter enum order and keeps historical session keys in manual
/// order, so an exact-equality check would never match and the overlay would
/// stick, masking later server changes). A safety timeout
/// (`OVERLAY_RECONCILE_TIMEOUT_MS`) drops a stale overlay if a send is dropped
/// and no snapshot ever arrives. See `dev-docs/26-agent-organization.md`
/// §4.3 / §7.4 / §12.1.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentsViewOverlay {
    pub filters: Option<AgentsViewFilters>,
    pub sort_mode: Option<AgentSortMode>,
    pub group_mode: Option<AgentGroupMode>,
    pub density: Option<AgentListDensity>,
    /// Deprecated protocol preference retained in the overlay shape for
    /// compatibility; current UI no longer sets or applies it.
    pub hide_finished: Option<bool>,
    pub manual_order: Option<Vec<AgentOrderKey>>,
    /// Optimistic override for the server-owned sidebar selectors (hide
    /// inactive / hide sub-agents / project visibility). `Some` only while a
    /// `SetSidebarPreferences` send is in flight; dropped wholesale on the next
    /// authoritative snapshot like every other domain.
    pub sidebar: Option<AgentsSidebarPreferences>,
    /// Optimistic override for the active Smart View id (dev-docs/26 §12.4):
    /// selecting a view sets the inner value to `Some(id)` so the switcher
    /// highlights instantly, while editing the query directly sets it to `None`
    /// so the highlight clears (the query no longer matches a named view). The
    /// outer `Option` follows the same domain-overlay convention as the other
    /// fields: `None` means "no override, read the server snapshot". Dropped
    /// wholesale on the next authoritative snapshot like every other domain.
    pub active_view_id: Option<Option<SmartViewId>>,
}

impl AgentsViewOverlay {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Per-project filter state for the Sessions/History panel. Stored per
/// active project (keyed by `Option<ActiveProjectRef>`, where `None`
/// represents the Home project) so user toggles persist across project
/// switches for the life of the app.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionsPanelFilters {
    pub show_child_sessions: bool,
    pub show_other_projects: bool,
}

impl SessionsPanelFilters {
    pub fn defaults_for(project: Option<&ActiveProjectRef>) -> Self {
        Self {
            show_child_sessions: false,
            show_other_projects: project.is_none(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ActiveAgentRef {
    pub host_id: String,
    pub agent_id: AgentId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveTerminalRef {
    pub host_id: String,
    pub terminal_id: TerminalId,
}

/// In-flight `WorkbenchCreate` request awaiting the matching `ProjectNotify::
/// Upsert`. The dispatcher correlates by `(host_id, parent_project_id, branch)`
/// — see §3.3 of `dev-docs/18-workbenches.md` — and on a match switches the
/// active project to the new workbench id, then removes the entry. A
/// `CommandError` for `WorkbenchCreate` marks the oldest non-failed entry for
/// the host with the error message (the error carries no parent/branch
/// correlation); the create modal consumes errored entries to surface the
/// failure inline. Entries are time-bounded by
/// [`PENDING_WORKBENCH_CREATE_TTL_MS`] so a mis-correlated or orphaned entry
/// cannot linger and trigger a spurious active-project switch much later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWorkbenchCreate {
    pub host_id: String,
    pub parent_project_id: ProjectId,
    pub branch: GitBranchName,
    /// Wall-clock ms (`Date.now()`) when the request was sent. See
    /// [`PendingWorkbenchCreate::is_stale`].
    pub requested_at_ms: u64,
    /// Error message from a `CommandError` for `WorkbenchCreate` on this
    /// host. `None` while the create is still in flight.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWorkbenchRemove {
    pub host_id: String,
    pub project_id: ProjectId,
    pub project_name: String,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkbenchRemovePrompt {
    pub host_id: String,
    pub project_id: ProjectId,
    pub project_name: String,
    pub message: String,
}

/// How long an in-flight workbench create stays correlatable. Past this the
/// entry is purged on the next touch of `pending_workbench_creates`.
pub const PENDING_WORKBENCH_CREATE_TTL_MS: u64 = 5 * 60 * 1000;

impl PendingWorkbenchCreate {
    pub fn is_stale(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.requested_at_ms) > PENDING_WORKBENCH_CREATE_TTL_MS
    }
}

/// Current wall-clock in ms. Zero on non-wasm builds (native logic tests
/// never exercise the staleness path).
pub fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

/// A pending request to run a workflow that declares inputs. The Workflows
/// panel Run button and the command palette both populate this; a global modal
/// renders one field per declared input and triggers the run on submit. A
/// workflow with no declared inputs never produces one of these — it runs in a
/// single click without a modal.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRunRequest {
    pub host_id: String,
    pub workflow_id: WorkflowId,
    pub project_id: Option<ProjectId>,
    pub name: String,
    pub inputs: Vec<WorkflowInputSpec>,
}

/// A workflow command failure surfaced inline in the Workflows panel. Keyed by
/// host. `request_kind` is the originating frame (`WorkflowRefresh`,
/// `TriggerWorkflow`, or `CancelWorkflow`) so the panel clears it on the next
/// successful notify for that operation.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowPanelError {
    pub request_kind: FrameKind,
    pub message: String,
}

/// Everything one chat's composer owns: the unsent text, the identity that text
/// is checkpointed under, and the spawn choices a draft chat would be created
/// with.
///
/// One handle per chat *tab*, not per agent — a "New Chat" tab has no agent yet
/// but still holds a draft, and a tab keeps its handle when its agent identity
/// is swapped underneath it (team-member upgrade, compaction retarget), so the
/// user's unsent text survives those.
///
/// `ArcRwSignal` rather than `RwSignal`: handles are created and dropped with
/// tabs at runtime, and the arena-allocated `RwSignal` would leak a slot per
/// closed tab.
#[derive(Clone)]
pub struct ComposerHandle {
    pub text: ArcRwSignal<String>,
    pub backend_override: ArcRwSignal<Option<BackendKind>>,
    pub custom_agent_id: ArcRwSignal<Option<CustomAgentId>>,
    pub launch_profile_id: ArcRwSignal<Option<LaunchProfileId>>,
    pub session_settings: ArcRwSignal<SessionSettingsValues>,
    pub session_settings_dirty: ArcRwSignal<bool>,
    /// Host the spawn choices above were made against. A profile id means
    /// nothing in another host's catalog, so switching hosts drops them.
    pub selection_host: ArcRwSignal<Option<String>>,
    /// Conversation identity `text` is currently checkpointed under. Tracked
    /// per tab so a pane whose chat is retargeted re-files its draft without
    /// disturbing the other pane's.
    #[cfg(target_arch = "wasm32")]
    draft_owner: ArcRwSignal<Option<PersistedComposerDraftOwner>>,
}

impl Default for ComposerHandle {
    fn default() -> Self {
        Self {
            text: ArcRwSignal::new(String::new()),
            backend_override: ArcRwSignal::new(None),
            custom_agent_id: ArcRwSignal::new(None),
            launch_profile_id: ArcRwSignal::new(None),
            session_settings: ArcRwSignal::new(SessionSettingsValues::default()),
            session_settings_dirty: ArcRwSignal::new(false),
            selection_host: ArcRwSignal::new(None),
            #[cfg(target_arch = "wasm32")]
            draft_owner: ArcRwSignal::new(None),
        }
    }
}

impl ComposerHandle {
    /// Reset the spawn choices while leaving the typed text alone.
    ///
    /// The selection is **one** host-bound value, not a bag of independent
    /// ones: backend, launch profile, custom agent, and profile-derived session
    /// settings are all chosen against a single host and are all meaningless
    /// against another, so they are always cleared as a unit.
    pub fn clear_selection(&self) {
        self.backend_override.set(None);
        self.launch_profile_id.set(None);
        self.custom_agent_id.set(None);
        self.session_settings.set(SessionSettingsValues::default());
        self.session_settings_dirty.set(false);
        self.selection_host.set(None);
    }
}

#[derive(Clone)]
pub struct AppState {
    pub configured_hosts: RwSignal<Vec<ConfiguredHost>>,
    pub selected_host_id: RwSignal<Option<String>>,
    pub host_streams: RwSignal<HashMap<String, StreamPath>>,
    pub connection_statuses: RwSignal<HashMap<String, ConnectionStatus>>,
    pub host_lifecycle_statuses: RwSignal<HashMap<String, RemoteHostLifecycleStatus>>,
    pub command_errors_by_host: RwSignal<HashMap<String, String>>,
    pub native_voice_supported: RwSignal<bool>,
    pub voice_capabilities_by_host: RwSignal<HashMap<String, protocol::VoiceCapabilitiesPayload>>,
    pub voice_ui: RwSignal<crate::voice::VoiceUiState>,
    pub voice_generation: RwSignal<u64>,
    /// Sticky user choice between the expanded voice band and the one-line
    /// strip. Never changed by session state — a view the user chose must
    /// not move on its own.
    pub voice_band_collapsed: RwSignal<bool>,
    /// Whether finalized voice transcripts are also appended to the agent
    /// chat. Off by default: the voice band is the voice conversation's
    /// home, and mixing it into the agent thread was reported as confusing.
    pub voice_transcript_in_chat: RwSignal<bool>,
    pub projects: RwSignal<Vec<ProjectInfo>>,
    pub agents: RwSignal<Vec<AgentInfo>>,
    pub sessions: RwSignal<Vec<SessionInfo>>,
    pub active_project: RwSignal<Option<ActiveProjectRef>>,
    /// Cold-start selection restored only after its owning host has published
    /// the project catalog. Catalog ownership and UI navigation are distinct;
    /// bootstrap must not guess the latter from whichever agent arrives first.
    pub pending_active_project_restore: RwSignal<Option<ActiveProjectRef>>,
    /// The chat to reopen after its owning host's bootstrap arrives, loaded
    /// from storage at construction. Held pending rather than applied eagerly
    /// because the agent it names only exists once that bootstrap lands.
    pub pending_active_chat_restore: RwSignal<Option<PersistedChatRef>>,
    /// The draft backend/profile choice to re-apply once its own host has
    /// bootstrapped. Held pending rather than seeded directly: the choice is
    /// only valid against that host's enabled backends and launch catalog, and
    /// neither exists until the bootstrap lands.
    pub pending_draft_restore: RwSignal<Option<PersistedDraftSelection>>,
    /// Latest paging state per `(host, scope)`, so a continuation can be both
    /// requested and *verified*. Scope is part of the key because cursors are
    /// scope-local: a page from another scope describes a different result set
    /// and its rows must not be merged into this one.
    pub session_list_pages: RwSignal<HashMap<SessionListPageKey, protocol::SessionListPageInfo>>,
    /// Hosts with a first-page refresh already in flight, so a burst of
    /// activity updates for unfetched sessions cannot become a burst of list
    /// requests.
    pub session_list_refresh_in_flight: RwSignal<HashSet<String>>,
    /// Derived from `center_zone.composer_owner()`. The focused pane's active
    /// chat wins; when a file pane is focused, the other pane's active chat
    /// remains the singleton composer owner. Read-only by design.
    pub active_agent: Memo<Option<ActiveAgentRef>>,
    pub chat_rows: RwSignal<HashMap<AgentId, Vec<ChatRowHandle>>>,
    pub chat_tool_rows: RwSignal<HashMap<AgentId, HashMap<ToolCallId, ChatRowId>>>,
    /// Per-agent index from server-issued `ChatMessageId` to the row that
    /// carries it. Populated by `push_chat_entry` when the entry's
    /// `message.message_id` is present, and consulted when a
    /// `ChatEvent::MessageMetadataUpdated` arrives so the existing row's
    /// token/model/context fields can be patched in place instead of
    /// appending a duplicate row. Cleared anywhere `chat_rows` is cleared
    /// (host runtime reset, agent close, agent bootstrap snapshot).
    pub chat_message_rows: RwSignal<HashMap<AgentId, HashMap<ChatMessageId, ChatRowId>>>,
    /// Server-owned prior-history availability for each agent. The server
    /// sends only this indicator in `AgentBootstrap`; actual prior transcript
    /// rows are fetched explicitly with `FetchSessionHistory` and prepended
    /// when `SessionHistory` arrives.
    pub session_history: RwSignal<HashMap<AgentId, SessionHistoryState>>,
    pub streaming_text: RwSignal<HashMap<AgentId, StreamingState>>,
    /// Latest `ToolProgress` snapshot per tool call, keyed by the owning
    /// agent and tool call id. The single source of truth for live tool
    /// activity (workflow runs, sub-agents): tool cards and the workflow
    /// tab look snapshots up reactively here — progress is deliberately
    /// NOT stored on `ToolRequestEntry`, which keyed `<For>` rows would
    /// render as a frozen snapshot. The inner signal lets an open card
    /// update without re-rendering the whole map. Cleared anywhere
    /// `chat_tool_rows` is cleared.
    pub tool_progress: RwSignal<HashMap<(AgentId, ToolCallId), ArcRwSignal<ToolProgressData>>>,
    /// Per-chat composer state, keyed by the owning chat tab. Deliberately a
    /// plain map and not a signal: the *lookup* never needs to be reactive
    /// because each `ChatView` binds to one fixed `TabId`, and the reactivity
    /// that matters already lives inside each handle. Making the map itself a
    /// signal would mean lazily creating a handle during a render that same
    /// render tracks.
    composers: Arc<Mutex<HashMap<TabId, ComposerHandle>>>,
    /// Composer used when no chat tab exists to own one (two files split, Home,
    /// cold start). Keeps every accessor total so callers never branch on
    /// "there is no composer".
    detached_composer: ComposerHandle,
    #[cfg(target_arch = "wasm32")]
    composer_drafts: RwSignal<PersistedComposerDraftStore>,
    #[cfg(target_arch = "wasm32")]
    composer_draft_persistence: Arc<ComposerDraftPersistenceRegistration>,
    #[cfg(target_arch = "wasm32")]
    composer_draft_limit_notified: RwSignal<bool>,
    #[cfg(target_arch = "wasm32")]
    composer_draft_pending_evictions: RwSignal<usize>,
    #[cfg(target_arch = "wasm32")]
    composer_draft_eviction_notified: RwSignal<bool>,
    #[cfg(target_arch = "wasm32")]
    composer_draft_persistence_failure_notified: RwSignal<bool>,
    pub task_lists: RwSignal<HashMap<AgentId, TaskList>>,
    /// Per-agent Tycode orchestration event log (sub-agent/workflow progress),
    /// chronological. Appended to as `ChatEvent::Orchestration` events arrive
    /// and as history replays; the orchestration panel folds it into a compact
    /// progress tree. These are Tycode-internal orchestration nodes, not
    /// first-class Tyde agents. Cleared with the agent's other per-agent state.
    pub orchestration: RwSignal<HashMap<AgentId, Vec<OrchestrationRecord>>>,
    /// Server-owned per-agent activity stats (running tool-call count, token
    /// usage, last output line), keyed by the owning `(host_id, agent_id)` so two
    /// hosts that hand out the same agent-id string can't collide. Populated from
    /// `AgentActivityStats` frames and agent bootstrap; the frontend renders
    /// these verbatim and never derives tool/token counts from chat rows. Cleared
    /// on agent close and host disconnect.
    pub agent_activity_stats: RwSignal<HashMap<ActiveAgentRef, AgentActivityStats>>,
    /// Server-authoritative task token rollups (root agent + descendants),
    /// keyed by the owning `(host_id, root_agent_id)`. Populated from
    /// `TaskTokenUsage` host-stream frames and host bootstrap; the frontend
    /// renders totals and breakdown rows verbatim and never sums entries
    /// itself. Cleared on agent close and host disconnect; a host bootstrap
    /// replaces the host's full set.
    pub task_token_usage: RwSignal<HashMap<ActiveAgentRef, TaskTokenUsagePayload>>,
    pub center_zone: RwSignal<CenterZoneState>,
    /// Window-local split ratio preference. The active project's split also
    /// carries its own ratio inside CenterLayout; only this scalar is suitable
    /// for cold local-storage persistence.
    pub center_split_ratio: RwSignal<SplitRatio>,
    /// Tabs whose content components are currently mounted, MRU-first. The
    /// active tab is always at the front; the next slot (if any) is the most
    /// recently active tab before it. Tabs absent from this list have their
    /// content unmounted entirely — no DOM, no reactive subscriptions. This
    /// keeps "many tabs open" cheap: we pay for at most `TAB_LRU_CAPACITY`
    /// component trees regardless of how many tabs the user has opened.
    /// Driven by an Effect in `App`; `mounted_tab_ids` additionally pins every
    /// pane's active tab so one pane cannot evict the other.
    pub tab_lru: RwSignal<Vec<TabId>>,
    pub tab_scroll_state: RwSignal<HashMap<TabId, TabScrollState>>,
    pub tabs_enabled: RwSignal<bool>,
    pub left_dock: RwSignal<DockVisibility>,
    pub right_dock: RwSignal<DockVisibility>,
    pub right_tab: RwSignal<RightTab>,
    pub bottom_dock: RwSignal<DockVisibility>,
    pub file_tree: RwSignal<HashMap<ProjectId, Vec<ProjectRootListing>>>,
    pub git_status: RwSignal<HashMap<ProjectId, Vec<ProjectRootGitStatus>>>,
    /// Server-authored code-intelligence overview per project (full-replacement
    /// frame `code_intel_overview`), keyed by the owning `(host_id, project_id)`
    /// so two hosts sharing a project-id string can't collide. Drives the
    /// Files-explorer status footer. The frontend renders this verbatim; it never
    /// infers provider state from open files or extensions.
    pub code_intel_overview: RwSignal<HashMap<ActiveProjectRef, CodeIntelOverviewPayload>>,
    pub open_files: RwSignal<HashMap<FileResourceKey, OpenFile>>,
    /// Invocation-time routing for cold file opens and in-place refreshes.
    /// Open wins over RefreshInPlace so a failed refresh marker cannot swallow
    /// the user's next explicit open.
    pub pending_file_opens: RwSignal<HashMap<FileResourceKey, PendingFileOpen>>,
    /// Server-pushed code-intelligence state, keyed by `(host_id, project_id,
    /// path)`. Kept separate from `Token`/syntax data on purpose (spec §6): the
    /// per-row token path has a wasm test guarding against text mangling, and
    /// semantic decorations must never ride that path.
    pub code_intel: RwSignal<HashMap<CodeIntelKey, CodeIntelFileState>>,
    /// Which diff tabs have pulled a file's contents + code-intel subscription
    /// for their own rows, keyed by the file. A diff tab is not a file tab, so
    /// without this the two lifetimes collide: closing a *file* tab would strip
    /// `open_files` and unsubscribe a file a diff tab is still hovering, and
    /// closing the *diff* tab would leave an orphan `didOpen` in the language
    /// server. Both directions consult this set before tearing anything down.
    pub diff_code_intel_holds: RwSignal<HashMap<CodeIntelKey, HashSet<TabId>>>,
    pub diff_contents: RwSignal<HashMap<DiffKey, DiffViewState>>,
    pub terminals: RwSignal<Vec<TerminalInfo>>,
    pub active_terminal: RwSignal<Option<ActiveTerminalRef>>,
    /// Agents whose interrupt has been sent but not yet acknowledged by a
    /// terminal event. Cancelling is a real phase of the turn lifecycle, not an
    /// instant: without it the Cancel control stays armed while the request is
    /// in flight, so repeated clicks send repeated Interrupt frames and the user
    /// gets no sign the first one was heard. Cleared by `OperationCancelled`, by
    /// any typing transition, and by agent teardown.
    pub interrupt_pending: RwSignal<HashSet<AgentId>>,
    /// Agents whose most recent turn ended in cancellation rather than
    /// completion, so the agent card can withhold the success affordance.
    ///
    /// Per-*turn* state, deliberately separate from the transcript row the same
    /// `OperationCancelled` event produces. The two were one record, which
    /// forced the visible notice to be dropped wholesale at the next turn in
    /// order to reset this flag — and that is what left the notice with no
    /// position of its own. Cleared by the next `TypingStatusChanged(true)` or
    /// `StreamStart`, by bootstrap, and by teardown.
    pub last_turn_cancelled: RwSignal<HashSet<AgentId>>,
    pub browse_dialog: RwSignal<Option<BrowseDialogState>>,
    /// Per-project snapshots of center-zone state. Updated whenever the user
    /// switches away from a project; consulted on switch-in to restore.
    pub project_view_memory: RwSignal<HashMap<ActiveProjectRef, ProjectViewMemory>>,
    pub command_palette_open: RwSignal<bool>,
    pub settings_open: RwSignal<bool>,
    /// When set, the settings panel jumps to the tab with this label (e.g.
    /// "Backends") the next time it renders. Used to deep-link from onboarding
    /// CTAs. Cleared by the panel once consumed.
    pub settings_tab_request: RwSignal<Option<&'static str>>,
    /// Current step of the guided help tour overlay, `None` when the tour is
    /// closed. The Help button on the home screen starts it at step 0.
    pub help_tour_step: RwSignal<Option<usize>>,
    pub feedback_open: RwSignal<bool>,
    pub find_bar_open: RwSignal<bool>,
    /// Which left-dock tab is active (Files / Git / Search).
    pub left_tab: RwSignal<LeftTab>,
    /// Persistent state for the project-wide Search panel.
    pub search_state: RwSignal<ProjectSearchUiState>,
    /// Persistent state for the find-references results panel (M5).
    pub references_state: RwSignal<ProjectReferencesUiState>,
    /// Bumped to request the Search panel focus (and select) its query input —
    /// e.g. on the Cmd/Ctrl+Shift+F shortcut or the "search in folder" action.
    pub search_focus_seq: RwSignal<u32>,
    /// When set, exactly one file occurrence should scroll so the given
    /// 1-based line is visible. Consumed by the matching TabId.
    pub pending_goto_line: RwSignal<Option<(TabId, u32)>>,
    /// Like `pending_goto_line` but addressed by an absolute file **byte
    /// offset** (from a go-to-definition target, whose range is byte-based). The
    /// file view converts it to a line via its `FileLines` and consumes it. Kept
    /// separate so the existing line-based goto machinery and its tests are
    /// untouched.
    pub pending_goto_offset: RwSignal<Option<(TabId, u32)>>,
    /// Monotonic source of `navigate_id` / `hover_id` domain ids for on-demand
    /// code-intel requests (cf. `search_id`). Bumped per request.
    pub code_intel_request_seq: RwSignal<u64>,
    /// Context for the most recent `code_intel_navigate` the client sent. A
    /// result is acted on only when it still matches this context (id + owning
    /// host/project + source file open at the same rendered version).
    pub code_intel_navigate_ctx: RwSignal<Option<CodeIntelNavigateContext>>,
    /// The most recent `hover_id` the client sent. Supersedes older hovers.
    pub code_intel_active_hover: RwSignal<u64>,
    /// The current hover popover, or `None` when nothing is hovered. The
    /// `HoverPopover` component renders from this signal (no `window.*`).
    pub code_intel_hover: RwSignal<Option<HoverPopover>>,
    /// Transient per-tab code-intel notice (see [`CodeIntelNotice`]); at most
    /// one at a time — a newer notice replaces the last.
    pub code_intel_notice: RwSignal<Option<CodeIntelNotice>>,
    /// True while the go-to-definition modifier is held. Mirrors the existing
    /// Cmd/Ctrl-click convention and is cleared on blur/visibility changes.
    pub cmd_held: RwSignal<bool>,
    /// The file (and rendered version) the user most recently interacted with in
    /// a file view, so the F12 keybinding (which has no file context of its own)
    /// can navigate from the current caret in that file.
    pub code_intel_focus: RwSignal<Option<FileFocus>>,
    pub host_settings_by_host: RwSignal<HashMap<String, HostSettings>>,
    pub host_settings_schema_by_host: RwSignal<HashMap<String, serde_json::Value>>,
    pub configured_secrets_by_host: RwSignal<HashMap<String, Vec<protocol::ConfiguredSecret>>>,
    pub backend_setup_by_host: RwSignal<HashMap<String, Vec<BackendSetupInfo>>>,
    pub agent_message_queue: RwSignal<HashMap<AgentId, Vec<QueuedMessageEntry>>>,
    pub agent_turn_active: RwSignal<HashMap<AgentId, bool>>,
    /// Server-owned launch profile catalog keyed by host id. Seeded by
    /// `HostBootstrap` and replaced wholesale by `LaunchProfileCatalogNotify`.
    /// The new-chat menus render these entries directly instead of deriving
    /// launch options from raw backend lists.
    pub launch_profile_catalog: RwSignal<HashMap<String, LaunchProfileCatalog>>,
    pub session_schemas: RwSignal<HashMap<String, HashMap<BackendKind, SessionSchemaEntry>>>,
    pub schemas_loaded_for_host: RwSignal<HashMap<String, bool>>,
    /// Host-level deep-config schemas, keyed by host id then backend kind.
    /// Backends without deep config are absent. Values live in
    /// `host_settings_by_host` (`HostSettings.backend_config`).
    pub backend_config_schemas:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendConfigSchema>>>,
    /// Server-owned snapshots of each backend's *current native* configuration,
    /// keyed by host id then backend kind. These are the backend's own source of
    /// truth (read by the server), distinct from the Tyde-managed overrides in
    /// `HostSettings.backend_config`. Backends without deep config are absent.
    pub backend_config_snapshots:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendConfigSnapshot>>>,
    /// Server-owned subscription-capacity snapshots, keyed by host id then
    /// backend kind. Replayed on host-stream subscribe and re-emitted on every
    /// change, so initial state and live updates travel the same path. Capacity
    /// is account-scoped but arrives on a per-agent pipe, so the server keys it
    /// by (host, backend) and the frontend must never key it by agent. The
    /// frontend renders these verbatim: it runs no freshness clock (staleness is
    /// the server's `CapacityFreshness` verdict), keeps no cache, and never
    /// infers capacity from Tyde's own token usage. Cleared on host disconnect.
    pub backend_capacity:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendCapacitySnapshot>>>,
    /// Server-owned backend-native settings snapshots (JSON-schema-driven,
    /// grouped), keyed by host id then backend kind. Each snapshot carries the
    /// backend's current settings document and grouped schemas, or an explicit
    /// unavailable status with a reason. Distinct from `backend_config_snapshots`
    /// (typed flat fields) and from the Tyde-managed overrides in
    /// `HostSettings.backend_config`. Backends without native settings are absent.
    pub backend_native_settings:
        RwSignal<HashMap<String, HashMap<BackendKind, protocol::BackendNativeSettingsSnapshot>>>,
    /// In-flight/failed state for backend-native settings saves, keyed by host id
    /// then backend kind. A native save sends the whole settings document, so a
    /// second edit based on the same (now stale) snapshot would clobber the first.
    /// While a save is `Pending`, the native controls are disabled until the
    /// server publishes a newer snapshot (detected by the settings document
    /// differing from the `base` the save was applied to) — so the "saving" state
    /// stays a projection of server-owned state, not an invented client model.
    pub native_settings_save_state:
        RwSignal<HashMap<String, HashMap<BackendKind, NativeSettingsSaveState>>>,
    /// Host id for which the next `NewTerminal` should steal focus. Set when the
    /// user clicks Install/Sign-in; consumed in the dispatcher so the new
    /// terminal becomes active even if another terminal was already selected.
    pub pending_terminal_focus: RwSignal<Option<String>>,
    pub agent_session_settings: RwSignal<HashMap<AgentId, SessionSettingsValues>>,
    /// User-visible settings submitted for a draft whose `NewAgent` echo has
    /// not arrived yet. The host stream publishes agent identity before the
    /// agent stream publishes authoritative effective settings; retaining the
    /// submitted values prevents that expected gap from masquerading as Auto.
    pub pending_agent_session_settings: RwSignal<PendingAgentSessionSettingsByProject>,
    next_pending_agent_session_settings_id: RwSignal<u64>,
    pub font_size: RwSignal<u32>,
    pub theme: RwSignal<String>,
    pub font_family: RwSignal<String>,
    /// Active syntect theme name (e.g. "base16-ocean.dark"). Drives both the
    /// file viewer and diff viewer's syntax coloring. Persists across sessions.
    pub syntax_theme: RwSignal<String>,
    pub diff_view_mode: RwSignal<DiffViewMode>,
    pub diff_context_mode: RwSignal<DiffContextMode>,
    pub tool_output_mode: RwSignal<ToolOutputMode>,
    pub custom_agents: RwSignal<HashMap<String, HashMap<CustomAgentId, CustomAgent>>>,
    pub mcp_servers: RwSignal<HashMap<String, HashMap<McpServerId, McpServerConfig>>>,
    pub steering: RwSignal<HashMap<String, HashMap<SteeringId, Steering>>>,
    pub skills: RwSignal<HashMap<String, HashMap<SkillId, Skill>>>,
    pub workflow_summaries: RwSignal<HashMap<String, Vec<WorkflowSummary>>>,
    pub workflow_diagnostics: RwSignal<HashMap<String, Vec<WorkflowDiagnostic>>>,
    pub workflow_runs: RwSignal<HashMap<String, HashMap<WorkflowRunId, WorkflowRunSnapshot>>>,
    /// Server-sent workflow catalog directories (global + per project root),
    /// keyed by host_id. Seeded by `HostBootstrap` and replaced wholesale by
    /// `WorkflowNotify`. The empty-state teaching copy and the authoring CTA
    /// read the real paths from here instead of reconstructing `.tyde/workflows`
    /// by string convention.
    pub workflow_locations: RwSignal<HashMap<String, Vec<WorkflowCatalogLocation>>>,
    /// Pending run-with-inputs request driving the global workflow inputs modal.
    /// `Some` while the modal is open; cleared on submit or cancel.
    pub workflow_run_request: RwSignal<Option<WorkflowRunRequest>>,
    /// Inline workflow command failures, keyed by host_id. Written by the
    /// `CommandError` dispatch path for workflow request kinds and cleared on the
    /// next successful workflow notify for the failed operation.
    pub workflow_command_errors: RwSignal<HashMap<String, WorkflowPanelError>>,
    /// Host-scoped team records, keyed by host_id then TeamId. Populated from
    /// `TeamNotify::Upsert` and pruned by `TeamNotify::Delete`.
    pub teams: RwSignal<HashMap<String, HashMap<TeamId, Team>>>,
    /// Host-scoped team member records. Members are looked up by id when
    /// rendering rosters and detail views; teams are joined via member.team_id.
    pub team_members: RwSignal<HashMap<String, HashMap<TeamMemberId, TeamMember>>>,
    /// Runtime team-member bindings: `current_agent_id`, status, last-active.
    /// Server emits these as `TeamMemberBindingNotify`. After a restart every
    /// binding starts with `current_agent_id: None` until the member is
    /// reactivated.
    pub team_member_bindings:
        RwSignal<HashMap<String, HashMap<TeamMemberId, TeamMemberBindingPayload>>>,
    /// Server-owned team creation catalog records. The frontend renders these
    /// options but does not define preset/template semantics locally.
    pub team_preset_catalogs: RwSignal<HashMap<String, TeamPresetCatalog>>,
    /// Server-owned in-progress team drafts, keyed by host then draft id.
    pub team_drafts: RwSignal<HashMap<String, HashMap<TeamDraftId, TeamDraft>>>,
    /// Latest server-emitted Add-report shuffle suggestion per host/team.
    /// The frontend bumps `serial` each time a notify arrives so the open
    /// dialog can detect a fresh suggestion and apply it without
    /// re-applying stale ones on re-open. Suggestions are ephemeral
    /// (never replayed on host attach).
    pub team_member_shuffle_suggestions:
        RwSignal<HashMap<String, HashMap<TeamId, TeamMemberShuffleSuggestionEntry>>>,
    /// Durable Agents-tab view preferences (filters, sort, group, density,
    /// manual order, plus deprecated protocol fields). The server is the single
    /// source of truth:
    /// the primary local host emits a `Some` snapshot in its bootstrap and via
    /// `AgentsViewPreferencesNotify`. This signal is *not* pruned on host
    /// cleanup, so a remount/reconnect re-reads the same server-fed base rather
    /// than re-deriving a fresh local map — the root fix for the Agents-tab
    /// flicker. See `dev-docs/26-agent-organization.md` §5.2 / §8.
    pub agents_view_preferences: RwSignal<AgentsViewPreferencesSnapshot>,
    /// Configured-host id of the primary local host that owns
    /// `agents_view_preferences`. Set when a bootstrap/notify carries a `Some`
    /// snapshot; preference mutations are routed back to this host's stream.
    pub agents_view_preferences_host: RwSignal<Option<String>>,
    /// Non-persisted optimistic overlay for in-flight preference mutations.
    pub pending_agents_view_overlay: RwSignal<AgentsViewOverlay>,
    /// Monotonic generation bumped on every overlay mutation. The safety
    /// timeout captures the generation it armed for and only drops the overlay
    /// if no newer mutation has since superseded it.
    pub agents_view_overlay_generation: RwSignal<u64>,
    pub sessions_panel_filters: RwSignal<HashMap<Option<ActiveProjectRef>, SessionsPanelFilters>>,
    /// Per-review full state. Server is the source of truth: a `ReviewView`
    /// subscribes to `/review/<id>` and dispatch applies `ReviewEvent`
    /// deltas to the entry. The first event on subscribe is always
    /// `ReviewEvent::Snapshot` which seeds (or replaces) the entry.
    pub reviews: RwSignal<HashMap<ReviewId, Review>>,
    /// Per-project review summary lists, populated from
    /// `ProjectEventPayload::ReviewListChanged` on each project stream.
    /// Used by the project rail / git panel indicator to show "open
    /// review against this working tree" without subscribing to every
    /// `/review/<id>` stream.
    pub review_summaries: RwSignal<HashMap<ProjectId, Vec<ReviewSummary>>>,
    /// True while a `ReviewCreate` for the given (host, project) is in
    /// flight and the server hasn't yet echoed a `ReviewListChanged` that
    /// includes a fresh review. Disables the "Review changes" button on
    /// the agent header so the user can't fire a second creation while
    /// the first is mid-flight. Cleared by the dispatch handler when a
    /// summary list refresh arrives that wasn't already known. No
    /// optimistic UI: we never synthesize a Review record on the
    /// frontend.
    pub review_create_pending: RwSignal<HashMap<(String, ProjectId), u32>>,
    /// Per-review action gate: true while a `ReviewAction` is in flight
    /// for that review id, used to disable buttons until the server
    /// echoes back the corresponding event. Each entry is a small bitmap
    /// of the actions awaiting echo so independent buttons (Submit,
    /// Cancel, Run AI, …) gate independently.
    pub review_action_pending: RwSignal<HashMap<ReviewId, ReviewActionGate>>,
    /// Per-(review, target) gate for actions that operate on a specific
    /// comment, suggestion, or composer instance. Held in a `HashSet` so
    /// each in-flight action keys to its own row, allowing independent
    /// rows to gate independently. Entries are cleared by dispatch when
    /// the matching `ReviewEvent` echoes back, or on
    /// `ReviewEvent::Error` whose context matches.
    pub review_action_target_pending: RwSignal<HashSet<(ReviewId, ReviewActionTarget)>>,
    /// Agents whose compaction request is in flight, keyed by the old
    /// agent id, with a snapshot of identifying fields captured at
    /// compaction-start time. The fingerprint lets the `NewAgent`
    /// dispatcher tell which incoming user-origin agent is the
    /// replacement (and so should NOT auto-open a competing tab) versus
    /// an unrelated spawn. The Agents panel renders these agents with a
    /// running-blue "Compacting…" pill and hides the Compact button so
    /// the user can't double-fire. Cleared by
    /// `finish_compaction_success` / `finish_compaction_failure`.
    pub compaction_in_progress: RwSignal<HashMap<AgentId, CompactionOldInfo>>,
    /// Last non-fatal compaction error per agent, keyed by the agent the
    /// user asked to compact. Rendered as an inline message on the agent
    /// card; cleared on the next successful start.
    pub compaction_errors: RwSignal<HashMap<AgentId, String>>,
    /// `Completed` notify can arrive before the replacement's `NewAgent`
    /// echo is dispatched. When that happens we stash `new → old` here
    /// keyed by `(host_id, new_agent_id)`, and the `NewAgent` arm flushes
    /// the entry by calling `finish_compaction_success`.
    pub compaction_pending_completion: RwSignal<HashMap<(String, AgentId), AgentId>>,
    /// Defensive belt for ordering inversions. Under the current
    /// server contract the event order is `NewAgent (replacement) →
    /// Completed (on old, still-valid stream) → AgentClosed (old)`,
    /// so by the time `AgentClosed` lands `compaction_in_progress`
    /// has already been cleared by `Completed` and the deferred-close
    /// set stays empty. We keep the set so that if the server ever
    /// inverts ordering for any reason — `AgentClosed` before
    /// `Completed` — we still preserve the user's chat tab until
    /// `Completed` retargets it. Drained at
    /// `finish_compaction_success` time.
    pub compaction_pending_close: RwSignal<HashSet<(String, AgentId)>>,
    /// Transient context-compaction operation per agent — the banner, the busy
    /// pill, and the duplicate-submit gate all read this.
    ///
    /// Distinct from `compaction_in_progress` above: that map tracks the
    /// *legacy replacement* protocol, where compaction swaps the agent. A
    /// context-compaction operation keeps the agent and logical session ids
    /// stable and never produces a `NewAgent`, so it cannot use the same
    /// state without inheriting a completion path that waits forever for a
    /// replacement that is never coming.
    pub context_compactions: RwSignal<HashMap<AgentId, ContextCompactionUiState>>,
    /// Operation ids already known terminal, per agent. Guards against a
    /// delayed progress frame or a duplicate terminal re-opening the banner.
    pub terminal_compaction_operations: RwSignal<HashMap<AgentId, TerminalOperationIds>>,
    /// Durable marker rows by marker id, per agent.
    ///
    /// The dedup key for the timeline marker. One compaction can reach this
    /// client from four directions — live, bootstrap replay, a paged history
    /// window, and a one-time legacy provider import — and every one of them
    /// carries the same provider-derived `marker_id`. Indexing by it is what
    /// makes "exactly one row per compaction" hold without content matching.
    /// Cleared everywhere `chat_rows` is cleared.
    pub context_compaction_rows:
        RwSignal<HashMap<AgentId, HashMap<CompactionObservationId, ChatRowId>>>,
    /// Latest typed team compaction result per `(host, team)`.
    ///
    /// Distinct from `team_compactions` on the legacy replacement path: this
    /// one carries stable per-member agent ids and typed statuses, and the team
    /// never swaps agents.
    pub team_context_compactions:
        RwSignal<HashMap<(String, TeamId), protocol::TeamContextCompactionNotifyPayload>>,
    /// Server-declared availability of a requested compaction, per agent.
    ///
    /// Capability answers "is this route offerable at all", never "is the
    /// agent free right now". A busy agent still accepts the request and the
    /// server reports it deferred, so momentary activity must not disable the
    /// control.
    pub compaction_capability: RwSignal<HashMap<AgentId, CompactionCapabilitySnapshot>>,
    /// Latest server-pushed `MobileAccessState` snapshot per host. The
    /// payload carries broker status, the pairing-lifecycle phase
    /// (`Idle | Active | Consumed | Expired | Cancelled | Failed`), and
    /// the paired-device list. The Mobile settings tab reads from this
    /// to render pairing status / device list. Server is the source of
    /// truth; the frontend never synthesises entries.
    pub mobile_access_state: RwSignal<HashMap<String, MobileAccessStatePayload>>,
    /// Latest server-pushed `MobilePairingOffer` per host. Contains the
    /// `qr_uri` we render as a QR code. Cleared when the pairing
    /// lifecycle transitions out of Active (Consumed / Expired /
    /// Cancelled / Failed) so a stale QR isn't left lying around.
    pub mobile_pairing_offer: RwSignal<HashMap<String, MobilePairingOfferPayload>>,
    /// Per-host bit: true while a `MobilePairingStart` is in flight and
    /// we haven't yet seen the server-confirmed offer back. Used to
    /// disable the Start button so the user can't double-fire while
    /// the server is preparing the offer.
    pub mobile_pairing_start_pending: RwSignal<HashSet<String>>,
    /// In-flight `WorkbenchCreate` requests. The dispatcher uses these to
    /// correlate the resulting `ProjectNotify::Upsert` and switch the active
    /// project to the freshly-created workbench. See `PendingWorkbenchCreate`.
    pub pending_workbench_creates: RwSignal<Vec<PendingWorkbenchCreate>>,
    /// In-flight workbench removals. A host-scoped `CommandError` is paired
    /// with the oldest entry for that host so the failure can be shown next
    /// to the destructive action instead of disappearing into header status.
    pub pending_workbench_removes: RwSignal<Vec<PendingWorkbenchRemove>>,
    pub workbench_remove_prompt: RwSignal<Option<WorkbenchRemovePrompt>>,
    /// Managed remote hosts for which the Phase 2 safety net has already fired
    /// its one forced upgrade-and-reconnect after a `Reject{IncompatibleProtocol}`.
    /// This is ephemeral, frontend-owned *connect-control* state — a one-shot
    /// guard scoped to the current connection lifecycle — NOT mirrored
    /// server/business state. It guarantees "upgrade once, no loop": cleared on a
    /// successful `Welcome` (so a later legitimate reconnect can retry once) and
    /// intended to be cleared on an explicit user disconnect via
    /// [`AppState::clear_upgrade_attempted`]. It is deliberately NOT cleared on a
    /// transport-drop disconnect, since that would let a server that keeps
    /// rejecting re-trigger the upgrade indefinitely.
    pub upgrade_attempted: RwSignal<HashSet<String>>,
}

/// Snapshot of identifying fields captured for an agent at the moment
/// its compaction was kicked off. Used by `dispatch::apply_new_agent` to
/// recognize the server-spawned replacement (which shares these fields)
/// without needing a protocol-level lineage flag on `NewAgentPayload`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionOldInfo {
    pub host_id: String,
    pub project_id: Option<ProjectId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub backend_kind: BackendKind,
    /// Team-member id is read from `team_member_bindings` at start
    /// time: if the old agent is the live binding for a team member,
    /// the replacement's `NewAgent` payload will carry the same
    /// member id, giving a deterministic match.
    pub team_member_id: Option<TeamMemberId>,
}

/// Identifier for a per-row review action awaiting server echo. Used as
/// part of a `(ReviewId, ReviewActionTarget)` key so independent rows
/// (different comments, suggestions) gate independently of each other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ReviewActionTarget {
    /// New comment via the inline composer for this review.
    AddComment,
    /// Update an existing comment.
    UpdateComment(ReviewCommentId),
    /// Delete an existing comment.
    DeleteComment(ReviewCommentId),
    /// Accept (or Edit & Accept) a pending AI suggestion.
    AcceptSuggestion(ReviewSuggestionId),
    /// Reject a pending AI suggestion.
    RejectSuggestion(ReviewSuggestionId),
}

/// Bitmask of review actions awaiting server echo. `0` means "nothing in
/// flight" — when the value drops back to `0` the entry can be removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReviewActionGate {
    pub submit: bool,
    pub cancel: bool,
    pub start_ai: bool,
    pub add_comment: bool,
    /// True while a `ClearComments` action is in flight, awaiting the
    /// server's `Cleared` echo. Gates the inline "Clear" control.
    pub clear: bool,
}

impl ReviewActionGate {
    pub fn is_idle(&self) -> bool {
        !(self.submit || self.cancel || self.start_ai || self.add_comment || self.clear)
    }
}

impl AppState {
    pub fn new() -> Self {
        let initial_center_zone = CenterZoneState::default();
        // Pre-seed the LRU with the initial active tab so the first
        // CenterZone render mounts it immediately. Without this seed the
        // first frame paints with no mounted tab content, then the
        // tab-LRU Effect in `App` fires and adds the active tab — visible
        // as a one-frame flash of empty center zone on app boot.
        let initial_lru: Vec<TabId> = initial_center_zone
            .panes()
            .filter_map(|(_, pane)| pane.active_tab_id)
            .collect();
        let center_zone: RwSignal<CenterZoneState> = RwSignal::new(initial_center_zone);
        let restored_selection = load_workspace_selection();
        let active_agent: Memo<Option<ActiveAgentRef>> = Memo::new(move |_| {
            center_zone.with(|cz| {
                cz.composer_owner()
                    .and_then(|(_, tab_id)| cz.tab(tab_id))
                    .and_then(|tab| match &tab.content {
                        TabContent::Chat { agent_ref, .. } => agent_ref.clone(),
                        _ => None,
                    })
            })
        });

        Self {
            configured_hosts: RwSignal::new(Vec::new()),
            selected_host_id: RwSignal::new(None),
            host_streams: RwSignal::new(HashMap::new()),
            connection_statuses: RwSignal::new(HashMap::new()),
            host_lifecycle_statuses: RwSignal::new(HashMap::new()),
            command_errors_by_host: RwSignal::new(HashMap::new()),
            native_voice_supported: RwSignal::new(true),
            voice_capabilities_by_host: RwSignal::new(HashMap::new()),
            voice_ui: RwSignal::new(crate::voice::VoiceUiState::Idle),
            voice_generation: RwSignal::new(0),
            voice_band_collapsed: RwSignal::new(false),
            voice_transcript_in_chat: RwSignal::new(false),
            projects: RwSignal::new(Vec::new()),
            agents: RwSignal::new(Vec::new()),
            sessions: RwSignal::new(Vec::new()),
            active_project: RwSignal::new(None),
            pending_active_project_restore: RwSignal::new(load_active_project()),
            pending_active_chat_restore: RwSignal::new(restored_selection.active_chat.clone()),
            pending_draft_restore: RwSignal::new(restored_selection.draft.clone()),
            session_list_pages: RwSignal::new(HashMap::new()),
            session_list_refresh_in_flight: RwSignal::new(HashSet::new()),
            active_agent,
            chat_rows: RwSignal::new(HashMap::new()),
            chat_tool_rows: RwSignal::new(HashMap::new()),
            chat_message_rows: RwSignal::new(HashMap::new()),
            session_history: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            agent_activity_stats: RwSignal::new(HashMap::new()),
            task_token_usage: RwSignal::new(HashMap::new()),
            tool_progress: RwSignal::new(HashMap::new()),
            composers: Arc::new(Mutex::new(HashMap::new())),
            detached_composer: ComposerHandle::default(),
            #[cfg(target_arch = "wasm32")]
            composer_drafts: RwSignal::new(PersistedComposerDraftStore::default()),
            #[cfg(target_arch = "wasm32")]
            composer_draft_persistence: register_composer_draft_scheduler(),
            #[cfg(target_arch = "wasm32")]
            composer_draft_limit_notified: RwSignal::new(false),
            #[cfg(target_arch = "wasm32")]
            composer_draft_pending_evictions: RwSignal::new(0),
            #[cfg(target_arch = "wasm32")]
            composer_draft_eviction_notified: RwSignal::new(false),
            #[cfg(target_arch = "wasm32")]
            composer_draft_persistence_failure_notified: RwSignal::new(false),
            task_lists: RwSignal::new(HashMap::new()),
            orchestration: RwSignal::new(HashMap::new()),
            center_zone,
            center_split_ratio: RwSignal::new(SplitRatio::default()),
            tab_lru: RwSignal::new(initial_lru),
            tab_scroll_state: RwSignal::new(HashMap::new()),
            tabs_enabled: RwSignal::new(true),
            left_dock: RwSignal::new(DockVisibility::Visible),
            right_dock: RwSignal::new(DockVisibility::Visible),
            right_tab: RwSignal::new(RightTab::Agents),
            bottom_dock: RwSignal::new(DockVisibility::Hidden),
            file_tree: RwSignal::new(HashMap::new()),
            git_status: RwSignal::new(HashMap::new()),
            code_intel_overview: RwSignal::new(HashMap::new()),
            open_files: RwSignal::new(HashMap::new()),
            pending_file_opens: RwSignal::new(HashMap::new()),
            code_intel: RwSignal::new(HashMap::new()),
            diff_code_intel_holds: RwSignal::new(HashMap::new()),
            diff_contents: RwSignal::new(HashMap::new()),
            terminals: RwSignal::new(Vec::new()),
            active_terminal: RwSignal::new(None),
            interrupt_pending: RwSignal::new(HashSet::new()),
            last_turn_cancelled: RwSignal::new(HashSet::new()),
            browse_dialog: RwSignal::new(None),
            project_view_memory: RwSignal::new(HashMap::new()),
            command_palette_open: RwSignal::new(false),
            settings_open: RwSignal::new(false),
            settings_tab_request: RwSignal::new(None),
            help_tour_step: RwSignal::new(None),
            feedback_open: RwSignal::new(false),
            find_bar_open: RwSignal::new(false),
            left_tab: RwSignal::new(LeftTab::Files),
            search_state: RwSignal::new(ProjectSearchUiState::default()),
            references_state: RwSignal::new(ProjectReferencesUiState::default()),
            search_focus_seq: RwSignal::new(0),
            pending_goto_line: RwSignal::new(None),
            pending_goto_offset: RwSignal::new(None),
            code_intel_request_seq: RwSignal::new(0),
            code_intel_navigate_ctx: RwSignal::new(None),
            code_intel_active_hover: RwSignal::new(0),
            code_intel_hover: RwSignal::new(None),
            code_intel_notice: RwSignal::new(None),
            cmd_held: RwSignal::new(false),
            code_intel_focus: RwSignal::new(None),
            host_settings_by_host: RwSignal::new(HashMap::new()),
            host_settings_schema_by_host: RwSignal::new(HashMap::new()),
            configured_secrets_by_host: RwSignal::new(HashMap::new()),
            backend_setup_by_host: RwSignal::new(HashMap::new()),
            agent_message_queue: RwSignal::new(HashMap::new()),
            agent_turn_active: RwSignal::new(HashMap::new()),
            launch_profile_catalog: RwSignal::new(HashMap::new()),
            session_schemas: RwSignal::new(HashMap::new()),
            schemas_loaded_for_host: RwSignal::new(HashMap::new()),
            backend_config_schemas: RwSignal::new(HashMap::new()),
            backend_config_snapshots: RwSignal::new(HashMap::new()),
            backend_capacity: RwSignal::new(HashMap::new()),
            backend_native_settings: RwSignal::new(HashMap::new()),
            native_settings_save_state: RwSignal::new(HashMap::new()),
            pending_terminal_focus: RwSignal::new(None),
            agent_session_settings: RwSignal::new(HashMap::new()),
            pending_agent_session_settings: RwSignal::new(HashMap::new()),
            next_pending_agent_session_settings_id: RwSignal::new(0),
            font_size: RwSignal::new(13),
            theme: RwSignal::new("dark".to_owned()),
            font_family: RwSignal::new("system".to_owned()),
            syntax_theme: RwSignal::new(crate::syntax_highlight::DEFAULT_THEME_NAME.to_owned()),
            diff_view_mode: RwSignal::new(DiffViewMode::Unified),
            diff_context_mode: RwSignal::new(DiffContextMode::Hunks),
            tool_output_mode: RwSignal::new(ToolOutputMode::Compact),
            custom_agents: RwSignal::new(HashMap::new()),
            mcp_servers: RwSignal::new(HashMap::new()),
            steering: RwSignal::new(HashMap::new()),
            skills: RwSignal::new(HashMap::new()),
            workflow_summaries: RwSignal::new(HashMap::new()),
            workflow_diagnostics: RwSignal::new(HashMap::new()),
            workflow_runs: RwSignal::new(HashMap::new()),
            workflow_locations: RwSignal::new(HashMap::new()),
            workflow_run_request: RwSignal::new(None),
            workflow_command_errors: RwSignal::new(HashMap::new()),
            teams: RwSignal::new(HashMap::new()),
            team_members: RwSignal::new(HashMap::new()),
            team_member_bindings: RwSignal::new(HashMap::new()),
            team_preset_catalogs: RwSignal::new(HashMap::new()),
            team_drafts: RwSignal::new(HashMap::new()),
            team_member_shuffle_suggestions: RwSignal::new(HashMap::new()),
            agents_view_preferences: RwSignal::new(AgentsViewPreferencesSnapshot {
                preferences: AgentsViewPreferences::default(),
                sidebar: Default::default(),
                load_error: None,
                smart_views: Default::default(),
                tags: Default::default(),
                pins: Default::default(),
                groups: Default::default(),
            }),
            agents_view_preferences_host: RwSignal::new(None),
            pending_agents_view_overlay: RwSignal::new(AgentsViewOverlay::default()),
            agents_view_overlay_generation: RwSignal::new(0),
            sessions_panel_filters: RwSignal::new(HashMap::new()),
            reviews: RwSignal::new(HashMap::new()),
            review_summaries: RwSignal::new(HashMap::new()),
            review_create_pending: RwSignal::new(HashMap::new()),
            review_action_pending: RwSignal::new(HashMap::new()),
            review_action_target_pending: RwSignal::new(HashSet::new()),
            compaction_in_progress: RwSignal::new(HashMap::new()),
            compaction_errors: RwSignal::new(HashMap::new()),
            compaction_pending_completion: RwSignal::new(HashMap::new()),
            compaction_pending_close: RwSignal::new(HashSet::new()),
            context_compactions: RwSignal::new(HashMap::new()),
            terminal_compaction_operations: RwSignal::new(HashMap::new()),
            context_compaction_rows: RwSignal::new(HashMap::new()),
            compaction_capability: RwSignal::new(HashMap::new()),
            team_context_compactions: RwSignal::new(HashMap::new()),
            mobile_access_state: RwSignal::new(HashMap::new()),
            mobile_pairing_offer: RwSignal::new(HashMap::new()),
            mobile_pairing_start_pending: RwSignal::new(HashSet::new()),
            pending_workbench_creates: RwSignal::new(Vec::new()),
            pending_workbench_removes: RwSignal::new(Vec::new()),
            workbench_remove_prompt: RwSignal::new(None),
            upgrade_attempted: RwSignal::new(HashSet::new()),
        }
    }

    /// Whether the Phase 2 safety net has already fired its one forced
    /// upgrade-and-reconnect for `host_id` on the current connection lifecycle.
    pub fn upgrade_already_attempted(&self, host_id: &str) -> bool {
        self.upgrade_attempted
            .with_untracked(|set| set.contains(host_id))
    }

    /// Record that the one-shot forced upgrade has fired for `host_id`. Blocks a
    /// second auto-upgrade until the guard is cleared (on `Welcome` or explicit
    /// disconnect), so the safety net can never loop.
    pub fn mark_upgrade_attempted(&self, host_id: &str) {
        self.upgrade_attempted.update(|set| {
            set.insert(host_id.to_owned());
        });
    }

    /// Clear the one-shot forced-upgrade guard for `host_id` so a later
    /// legitimate reconnect can attempt the upgrade once more. Called on a
    /// successful `Welcome`; should also be called from the explicit
    /// user-initiated disconnect path.
    pub fn clear_upgrade_attempted(&self, host_id: &str) {
        self.upgrade_attempted.update(|set| {
            set.remove(host_id);
        });
    }

    /// Record that the user has fired a compaction for `(host_id,
    /// agent_id)`. Looks up the agent's `AgentInfo` + team-member
    /// binding so the dispatcher can later correlate the replacement
    /// agent's `NewAgent` echo to this compaction without protocol-
    /// level lineage info. Clears any prior error so a fresh attempt
    /// has a clean error surface.
    pub fn mark_compaction_started(&self, host_id: &str, agent_id: AgentId) {
        self.compaction_errors.update(|m| {
            m.remove(&agent_id);
        });
        let info = self.compaction_info_for(host_id, &agent_id);
        self.compaction_in_progress.update(|map| {
            map.insert(agent_id, info);
        });
    }

    /// Build the fingerprint by reading the agent's own `AgentInfo` and
    /// scanning `team_member_bindings` for any member whose live
    /// `current_agent_id` matches. The team-member id (when present) is
    /// the strongest correlation field because the replacement's
    /// `NewAgent` payload always carries the same value.
    fn compaction_info_for(&self, host_id: &str, agent_id: &AgentId) -> CompactionOldInfo {
        let (project_id, custom_agent_id, backend_kind) = self.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == host_id && &a.agent_id == agent_id)
                .map(|a| {
                    (
                        a.project_id.clone(),
                        a.custom_agent_id.clone(),
                        a.backend_kind,
                    )
                })
                .unwrap_or((None, None, BackendKind::Claude))
        });
        let team_member_id = self.team_member_bindings.with_untracked(|map| {
            map.get(host_id).and_then(|members| {
                members.iter().find_map(|(member_id, binding)| {
                    if binding.current_agent_id.as_ref() == Some(agent_id) {
                        Some(member_id.clone())
                    } else {
                        None
                    }
                })
            })
        });
        CompactionOldInfo {
            host_id: host_id.to_owned(),
            project_id,
            custom_agent_id,
            backend_kind,
            team_member_id,
        }
    }

    /// Find an in-flight compaction whose old-agent fingerprint matches
    /// the new agent identified by `(host_id, fields)`. The dispatcher
    /// uses this in `apply_new_agent` to recognize the replacement and
    /// skip the auto-tab-open path that would otherwise steal focus
    /// from the user's existing chat tab.
    pub fn find_compaction_replacement(
        &self,
        host_id: &str,
        team_member_id: Option<&TeamMemberId>,
        project_id: Option<&ProjectId>,
        custom_agent_id: Option<&CustomAgentId>,
        backend_kind: BackendKind,
    ) -> Option<AgentId> {
        self.compaction_in_progress.with_untracked(|map| {
            for (old_id, info) in map.iter() {
                if info.host_id != host_id {
                    continue;
                }
                // Team-member match is decisive when both sides have a
                // member id: the replacement's NewAgent payload always
                // carries the same one.
                let team_match = match (info.team_member_id.as_ref(), team_member_id) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => true,
                    _ => false,
                };
                if !team_match {
                    continue;
                }
                if info.project_id.as_ref() != project_id {
                    continue;
                }
                if info.custom_agent_id.as_ref() != custom_agent_id {
                    continue;
                }
                if info.backend_kind != backend_kind {
                    continue;
                }
                return Some(old_id.clone());
            }
            None
        })
    }

    /// Add `(host_id, agent_id)` to the deferred-close set. Used by
    /// `dispatch::apply_agent_closed` when an `AgentClosed` arrives for
    /// an agent that is mid-compaction: we keep the agent's state
    /// alive so `finish_compaction_success` has something to retarget,
    /// and finalize the close from there.
    pub fn defer_compaction_close(&self, host_id: &str, agent_id: AgentId) {
        self.compaction_pending_close.update(|set| {
            set.insert((host_id.to_owned(), agent_id));
        });
    }

    /// Server-confirmed completion: the compaction finished, the
    /// predecessor is being closed, and `new_agent` is the live
    /// replacement. Retargets every chat tab pointing at `prev_agent_id`
    /// to `new_agent` so the user keeps working in the same tab without
    /// remount/focus churn — mirrors `upgrade_pending_team_member_tab`.
    pub fn finish_compaction_success(&self, prev_agent_id: &AgentId, new_agent: &AgentInfo) {
        self.compaction_in_progress.update(|map| {
            map.remove(prev_agent_id);
        });
        self.compaction_errors.update(|m| {
            m.remove(prev_agent_id);
        });
        let new_ref = ActiveAgentRef {
            host_id: new_agent.host_id.clone(),
            agent_id: new_agent.agent_id.clone(),
        };
        let label = new_agent.name.clone();
        let new_ref_for_memory = new_ref.clone();
        let label_for_memory = label.clone();
        let prev_for_cz = prev_agent_id.clone();
        self.center_zone.update(|cz| {
            cz.for_each_tab_mut(|_, tab| {
                if let TabContent::Chat {
                    agent_ref: Some(ar),
                    ..
                } = &tab.content
                    && ar.host_id == new_ref.host_id
                    && ar.agent_id == prev_for_cz
                {
                    tab.content = TabContent::chat_with_agent(new_ref.clone());
                    tab.label = label.clone();
                }
            });
        });
        let prev_for_memory = prev_agent_id.clone();
        self.project_view_memory.update(|map| {
            for memory in map.values_mut() {
                let Some(cz) = memory.center_zone.as_mut() else {
                    continue;
                };
                cz.for_each_tab_mut(|_, tab| {
                    if let TabContent::Chat {
                        agent_ref: Some(ar),
                        ..
                    } = &tab.content
                        && ar.host_id == new_ref_for_memory.host_id
                        && ar.agent_id == prev_for_memory
                    {
                        tab.content = TabContent::chat_with_agent(new_ref_for_memory.clone());
                        tab.label = label_for_memory.clone();
                    }
                });
            }
        });
        // Under the current server contract `AgentClosed` arrives
        // AFTER `Completed`, so the deferred-close set is normally
        // empty here and the cleanup below is a no-op — the normal
        // `apply_agent_closed` path will handle teardown. If the
        // server ever inverts ordering (AgentClosed before Completed),
        // the dispatcher's defer path will have queued the teardown
        // in `compaction_pending_close` and we drain it now, after
        // the retarget, so the old agent's transient state is gone.
        let prev_for_close = prev_agent_id.clone();
        let new_host = new_ref.host_id.clone();
        let had_pending_close = self
            .compaction_pending_close
            .with_untracked(|set| set.contains(&(new_host.clone(), prev_for_close.clone())));
        if had_pending_close {
            self.compaction_pending_close.update(|set| {
                set.remove(&(new_host.clone(), prev_for_close.clone()));
            });
            self.finalize_compaction_close(&new_host, &prev_for_close);
        }
    }

    /// Drop every transient state map entry tied to the closed old
    /// agent. Mirrors `dispatch::apply_agent_closed`'s cleanup set so
    /// the deferred-close path doesn't leave stale entries behind that
    /// the normal close path would have dropped. The tab-related steps
    /// (close any tab still pointing at the old agent + prune LRU) are
    /// belt-and-suspenders here: `finish_compaction_success` retargets
    /// every Chat tab from `old -> new` first, so by the time we reach
    /// this point the close-tabs sweep is typically a no-op. We keep
    /// it because nothing guarantees every surface was retargeted
    /// (e.g. a future tab type that `finish_compaction_success`
    /// doesn't know about), and leaving a stray tab pointing at a
    /// dead agent is worse than a redundant scan.
    /// Drop server-provided prior-history state for a single agent. Call
    /// wherever `chat_rows` is cleared for that agent so a re-bootstrap starts
    /// from the server's new authoritative indicator.
    pub fn forget_session_history(&self, agent_id: &AgentId) {
        self.session_history.update(|map| {
            map.remove(agent_id);
        });
    }

    // ── Context compaction ──────────────────────────────────────────────

    /// Optimistic local start, between the click and the first server frame.
    ///
    /// Returns `false` if a request or operation is already outstanding, which
    /// is how the header button, the agent card, the palette entry, and the
    /// team control share one gate: whichever fires first wins and the rest
    /// become no-ops. Without this, two controls both submit before either
    /// sees an operation id.
    pub fn begin_compaction_request(&self, agent_id: &AgentId) -> bool {
        let already = self
            .context_compactions
            .with_untracked(|map| map.get(agent_id).is_some_and(|state| state.is_in_flight()));
        // The legacy replacement protocol keeps its own in-flight record, and
        // it is still live. Arbitrating only the typed one would let the two
        // protocols each admit a request the other is already running, so this
        // gate spans both — every caller shares it, individual and team alike.
        let legacy_already = self
            .compaction_in_progress
            .with_untracked(|map| map.contains_key(agent_id));
        if already || legacy_already {
            return false;
        }
        self.context_compactions.update(|map| {
            map.insert(agent_id.clone(), ContextCompactionUiState::Requesting);
        });
        true
    }

    /// Transport failed before the server ever saw the request: drop the
    /// optimistic state so the control re-arms.
    pub fn abandon_compaction_request(&self, agent_id: &AgentId, message: String) {
        self.context_compactions.update(|map| {
            if matches!(
                map.get(agent_id),
                Some(ContextCompactionUiState::Requesting)
            ) {
                map.remove(agent_id);
            }
        });
        self.compaction_errors.update(|map| {
            map.insert(agent_id.clone(), message);
        });
    }

    /// Apply a server operation frame.
    ///
    /// Returns `true` when this frame is a *live terminal transition* the
    /// caller should announce. Bootstrap passes terminal frames through this
    /// too — they are filtered out server-side, but a client that trusts that
    /// silently is one contract change away from announcing history.
    pub fn apply_context_compaction_notify(
        &self,
        agent_id: &AgentId,
        payload: ContextCompactionNotifyPayload,
        live: bool,
    ) -> bool {
        let operation_id = payload.operation_id.clone();

        // A frame for an operation this client already saw finish is stale:
        // a delayed heartbeat, or a duplicate terminal. Either would otherwise
        // reopen a banner for work that is over.
        let already_terminal = self.terminal_compaction_operations.with_untracked(|map| {
            map.get(agent_id)
                .is_some_and(|seen| seen.contains(&operation_id))
        });
        if already_terminal {
            log::debug!(
                "context_compaction_notify ignored for terminal operation agent_id={agent_id} operation_id={}",
                operation_id.0
            );
            return false;
        }

        let terminal = payload.status.is_terminal();
        if terminal {
            self.terminal_compaction_operations.update(|map| {
                map.entry(agent_id.clone())
                    .or_default()
                    .insert(operation_id.clone());
            });
        }

        match &payload.status {
            ContextCompactionStatus::Failed { .. } => {
                let message = payload
                    .message
                    .clone()
                    .unwrap_or_else(|| "Compaction failed.".to_owned());
                self.compaction_errors.update(|map| {
                    map.insert(agent_id.clone(), message);
                });
                // Symmetrical with `Completed`, and for the same reason: the
                // durable marker row is the record of what happened, in the
                // place it happened. `compaction_errors` carries the provider's
                // prose to the agent card. Retaining an operation here as well
                // left a banner pinned to the end of the transcript for the
                // rest of the session.
                self.context_compactions.update(|map| {
                    map.remove(agent_id);
                });
            }
            ContextCompactionStatus::Completed => {
                // The durable marker is the record of success. Nothing is
                // retained here, and — unlike the legacy replacement path —
                // nothing waits for a `NewAgent` that is never coming.
                self.compaction_errors.update(|map| {
                    map.remove(agent_id);
                });
                self.context_compactions.update(|map| {
                    map.remove(agent_id);
                });
            }
            ContextCompactionStatus::Deferred { .. }
            | ContextCompactionStatus::Started { .. }
            | ContextCompactionStatus::Progress { .. } => {
                self.compaction_errors.update(|map| {
                    map.remove(agent_id);
                });
                self.context_compactions.update(|map| {
                    map.insert(
                        agent_id.clone(),
                        ContextCompactionUiState::Active {
                            payload: Box::new(payload),
                            live,
                        },
                    );
                });
            }
        }

        terminal
    }

    /// Drop everything compaction-related for one agent.
    ///
    /// Called wherever the transcript itself is dropped. The marker index is
    /// listed here because it points at `ChatRowId`s: leaving it behind after
    /// `chat_rows` is cleared makes the next sighting of a marker resolve to a
    /// row that no longer exists, and the marker silently never renders.
    pub fn forget_context_compaction(&self, agent_id: &AgentId) {
        self.context_compactions.update(|map| {
            map.remove(agent_id);
        });
        self.terminal_compaction_operations.update(|map| {
            map.remove(agent_id);
        });
        self.context_compaction_rows.update(|map| {
            map.remove(agent_id);
        });
        self.compaction_capability.update(|map| {
            map.remove(agent_id);
        });
    }

    /// Reset every compaction read-model record for an agent ahead of an
    /// authoritative `AgentBootstrap` replay.
    ///
    /// Bootstrap is a *replacement*, not a merge: whatever it omits is
    /// authoritatively absent. Keeping local state across it lets a
    /// pre-bootstrap `Requesting` persist forever (its operation frames went to
    /// the dead connection), a terminal `Failed` banner outlive the server's
    /// correct decision not to bootstrap terminal snapshots, and a capability
    /// snapshot from a previous logical session keep gating the new one.
    ///
    /// The pending history request goes too — it belonged to the connection
    /// that just went away, and leaving it outstanding would make the first
    /// page of the new connection look stale.
    pub fn reset_compaction_read_model(&self, agent_id: &AgentId) {
        self.forget_context_compaction(agent_id);
        self.session_history.update(|map| {
            if let Some(history) = map.get_mut(agent_id) {
                history.pending_request = None;
            }
        });
    }

    /// Store a capability snapshot, rejecting one that describes a different
    /// logical session than the agent is currently bound to.
    ///
    /// Returns `false` when the snapshot was rejected. Fail-closed: a rejected
    /// snapshot leaves capability *unknown*, which the control selector treats
    /// as "still determining" and disables, rather than inheriting the previous
    /// session's answer.
    pub fn apply_compaction_capability(
        &self,
        agent_id: &AgentId,
        snapshot: CompactionCapabilitySnapshot,
    ) -> bool {
        let bound_session = self.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|agent| agent.agent_id == *agent_id)
                .and_then(|agent| agent.session_id.clone())
        });
        if let Some(bound_session) = bound_session
            && bound_session != snapshot.logical_session_id
        {
            log::warn!(
                "context_compaction_capability for agent {} names a different session than the bound agent; dropping",
                agent_id.0,
            );
            self.compaction_capability.update(|map| {
                map.remove(agent_id);
            });
            return false;
        }
        self.compaction_capability.update(|map| {
            map.insert(agent_id.clone(), snapshot);
        });
        true
    }

    fn finalize_compaction_close(&self, host_id: &str, agent_id: &AgentId) {
        self.agents.update(|agents| {
            agents.retain(|agent| !(agent.host_id == host_id && agent.agent_id == *agent_id));
        });
        self.chat_rows.update(|map| {
            map.remove(agent_id);
        });
        self.forget_session_history(agent_id);
        self.forget_context_compaction(agent_id);
        self.chat_tool_rows.update(|map| {
            map.remove(agent_id);
        });
        self.tool_progress.update(|map| {
            map.retain(|(id, _), _| id != agent_id);
        });
        self.chat_message_rows.update(|map| {
            map.remove(agent_id);
        });
        self.streaming_text.update(|map| {
            map.remove(agent_id);
        });
        self.agent_activity_stats.update(|map| {
            map.remove(&ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: agent_id.clone(),
            });
        });
        self.task_token_usage.update(|map| {
            map.remove(&ActiveAgentRef {
                host_id: host_id.to_owned(),
                agent_id: agent_id.clone(),
            });
        });
        self.agent_turn_active.update(|map| {
            map.remove(agent_id);
        });
        self.interrupt_pending.update(|set| {
            set.remove(agent_id);
        });
        self.last_turn_cancelled.update(|set| {
            set.remove(agent_id);
        });
        self.task_lists.update(|map| {
            map.remove(agent_id);
        });
        self.orchestration.update(|map| {
            map.remove(agent_id);
        });
        self.agent_message_queue.update(|map| {
            map.remove(agent_id);
        });
        self.agent_session_settings.update(|map| {
            map.remove(agent_id);
        });
        let host_for_cz = host_id.to_owned();
        let agent_for_cz = agent_id.clone();
        let mut removed_tab_ids = HashSet::new();
        self.center_zone.update(|cz| {
            removed_tab_ids.extend(close_agent_tabs_in_cz(cz, &host_for_cz, &agent_for_cz));
        });
        let host_for_memory = host_id.to_owned();
        let agent_for_memory = agent_id.clone();
        self.project_view_memory.update(|memories| {
            for memory in memories.values_mut() {
                if let Some(center_zone) = memory.center_zone.as_mut() {
                    removed_tab_ids.extend(close_agent_tabs_in_cz(
                        center_zone,
                        &host_for_memory,
                        &agent_for_memory,
                    ));
                }
            }
        });
        self.forget_removed_tab_occurrence_state(&removed_tab_ids);
    }

    /// Server-confirmed failure: the compaction did not produce a
    /// replacement. The predecessor is still alive, so we just clear the
    /// in-flight flag and surface the message on its card. We also
    /// belt-and-suspenders drain the pending-close set in case it ever
    /// gets populated on a failure path.
    pub fn finish_compaction_failure(&self, agent_id: AgentId, message: String) {
        self.compaction_in_progress.update(|map| {
            map.remove(&agent_id);
        });
        let agent_id_for_close = agent_id.clone();
        self.compaction_pending_close.update(|set| {
            set.retain(|(_, a)| a != &agent_id_for_close);
        });
        self.compaction_errors.update(|m| {
            m.insert(agent_id, message);
        });
    }

    pub fn selected_host(&self) -> Option<ConfiguredHost> {
        let selected = self.selected_host_id.get()?;
        self.configured_hosts
            .get()
            .into_iter()
            .find(|host| host.id == selected)
    }

    /// Host that the currently visible chat controls should operate on.
    ///
    /// This intentionally differs from `selected_host_id`, which is the host
    /// selected in Settings. Existing chats are bound to their agent host; new
    /// chats opened while a project is active are bound to that project's host;
    /// only global/Home chats fall back to the Settings-selected host.
    pub fn chat_context_host_id(&self) -> Option<String> {
        if let Some(active_agent) = self.active_agent.get() {
            return Some(active_agent.host_id);
        }
        if let Some(active_project) = self.active_project.get() {
            return Some(active_project.host_id);
        }
        self.selected_host_id.get()
    }

    pub fn chat_context_host_id_untracked(&self) -> Option<String> {
        if let Some(active_agent) = self.active_agent.get_untracked() {
            return Some(active_agent.host_id);
        }
        if let Some(active_project) = self.active_project.get_untracked() {
            return Some(active_project.host_id);
        }
        self.selected_host_id.get_untracked()
    }

    pub fn connection_status_for_host(&self, host_id: &str) -> ConnectionStatus {
        self.connection_statuses
            .get()
            .get(host_id)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    pub fn host_settings(&self, host_id: &str) -> Option<HostSettings> {
        self.host_settings_by_host.get().get(host_id).cloned()
    }

    pub fn host_settings_untracked(&self, host_id: &str) -> Option<HostSettings> {
        self.host_settings_by_host
            .get_untracked()
            .get(host_id)
            .cloned()
    }

    pub fn chat_context_connection_status(&self) -> ConnectionStatus {
        let Some(host_id) = self.chat_context_host_id() else {
            return ConnectionStatus::Disconnected;
        };
        self.connection_status_for_host(&host_id)
    }

    pub fn chat_context_host_settings(&self) -> Option<HostSettings> {
        let host_id = self.chat_context_host_id()?;
        self.host_settings(&host_id)
    }

    pub fn chat_context_host_settings_untracked(&self) -> Option<HostSettings> {
        let host_id = self.chat_context_host_id_untracked()?;
        self.host_settings_untracked(&host_id)
    }

    pub fn host_stream_untracked(&self, host_id: &str) -> Option<StreamPath> {
        self.host_streams.get_untracked().get(host_id).cloned()
    }

    pub fn selected_host_stream_untracked(&self) -> Option<(String, StreamPath)> {
        let host_id = self.selected_host_id.get_untracked()?;
        let stream = self.host_stream_untracked(&host_id)?;
        Some((host_id, stream))
    }

    /// Reactively resolve the effective Agents-view preferences: the durable
    /// server snapshot with the non-persisted optimistic overlay layered on top
    /// per preference domain. Reads both signals, so callers inside a reactive
    /// closure re-run when either the server snapshot or the overlay changes.
    pub fn effective_agents_view_preferences(&self) -> AgentsViewPreferences {
        let base = self.agents_view_preferences.get().preferences;
        let overlay = self.pending_agents_view_overlay.get();
        AgentsViewPreferences {
            filters: overlay.filters.unwrap_or(base.filters),
            sort_mode: overlay.sort_mode.unwrap_or(base.sort_mode),
            group_mode: overlay.group_mode.unwrap_or(base.group_mode),
            density: overlay.density.unwrap_or(base.density),
            hide_finished: overlay.hide_finished.unwrap_or(base.hide_finished),
            manual_order: overlay.manual_order.unwrap_or(base.manual_order),
        }
    }

    /// Reactively resolve the effective sidebar selector preferences (hide
    /// inactive / hide sub-agents / project visibility): the durable server
    /// snapshot with the optimistic overlay layered on top. Reads both signals,
    /// so callers inside a reactive closure re-run when either changes.
    pub fn effective_agents_sidebar_preferences(&self) -> AgentsSidebarPreferences {
        self.pending_agents_view_overlay
            .get()
            .sidebar
            .unwrap_or_else(|| self.agents_view_preferences.get().sidebar)
    }

    /// Reactively resolve the active Smart View id: the optimistic overlay
    /// value when a view selection (or a divergent query edit) is in flight,
    /// otherwise the server snapshot's `active_view_id`. `None` means no view
    /// is highlighted — either the server reports a custom/divergent query or
    /// an in-flight edit cleared the highlight. Reads both signals so callers
    /// inside a reactive closure re-run on either change.
    pub fn effective_active_smart_view_id(&self) -> Option<SmartViewId> {
        match self.pending_agents_view_overlay.get().active_view_id {
            Some(active) => active,
            None => {
                self.agents_view_preferences
                    .get()
                    .smart_views
                    .active_view_id
            }
        }
    }

    /// Apply a server-emitted Agents-view preference snapshot. Only the primary
    /// local host owns these preferences (dev-docs/26 §12.1): a `Some` snapshot
    /// from any other host is ignored so a stray remote payload cannot hijack
    /// the client-global signal or its owner pointer.
    ///
    /// The snapshot is authoritative and full, so the optimistic overlay is
    /// dropped wholesale — the server wins even when its canonicalized value
    /// differs from the optimistic one (sorted filter enums, retained historical
    /// session keys). Matching the optimistic value exactly is impossible after
    /// canonicalization, so an equality-only reconcile would leave the overlay
    /// stuck and mask future server changes to that domain.
    pub fn apply_agents_view_snapshot(
        &self,
        host_id: &str,
        snapshot: AgentsViewPreferencesSnapshot,
    ) {
        if host_id != PRIMARY_LOCAL_HOST_ID {
            log::warn!("ignoring agents-view preferences snapshot from non-primary host {host_id}");
            return;
        }
        self.agents_view_preferences.set(snapshot);
        self.agents_view_preferences_host
            .set(Some(host_id.to_owned()));
        // A new authoritative snapshot supersedes every in-flight domain. Bump
        // the generation so any pending safety-timeout for the old overlay
        // becomes a no-op.
        self.agents_view_overlay_generation
            .update(|generation| *generation = generation.wrapping_add(1));
        self.pending_agents_view_overlay
            .set(AgentsViewOverlay::default());
    }

    /// Install an optimistic overlay update for an in-flight preference domain
    /// and run `mutate` on the overlay. Used right before a
    /// `SetAgentsViewPreferences` frame is sent so the UI reacts immediately.
    /// Arms a safety timeout so a dropped/failed send cannot freeze the view.
    pub fn set_agents_view_overlay(&self, mutate: impl FnOnce(&mut AgentsViewOverlay)) {
        self.pending_agents_view_overlay
            .update(|overlay| mutate(overlay));
        let generation = self
            .agents_view_overlay_generation
            .try_update(|generation| {
                *generation = generation.wrapping_add(1);
                *generation
            })
            .unwrap_or_default();
        self.arm_overlay_reconcile_timeout(generation);
    }

    /// Schedule the safety backstop: after `OVERLAY_RECONCILE_TIMEOUT_MS`, if no
    /// newer overlay mutation or authoritative snapshot has bumped the
    /// generation and the overlay is still pending, drop it. Uses `try_*`
    /// accessors so a timer that fires after the owning scope is disposed (e.g.
    /// across test boundaries) is a harmless no-op. No-op off wasm.
    #[cfg(target_arch = "wasm32")]
    fn arm_overlay_reconcile_timeout(&self, generation: u64) {
        use wasm_bindgen::JsCast;
        use wasm_bindgen::closure::Closure;

        let state = self.clone();
        let callback = Closure::once_into_js(move || {
            let still_current = state
                .agents_view_overlay_generation
                .try_get_untracked()
                .map(|current| current == generation)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let _ = state.pending_agents_view_overlay.try_update(|overlay| {
                if !overlay.is_empty() {
                    log::warn!("agents-view overlay timed out without server reconcile; dropping");
                    *overlay = AgentsViewOverlay::default();
                }
            });
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.unchecked_ref(),
                OVERLAY_RECONCILE_TIMEOUT_MS,
            );
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn arm_overlay_reconcile_timeout(&self, _generation: u64) {}

    /// True while any preference mutation is awaiting server confirmation.
    pub fn agents_view_overlay_pending(&self) -> bool {
        !self.pending_agents_view_overlay.get().is_empty()
    }

    /// Append a context-compaction marker row, or update the existing one.
    ///
    /// Returns `true` when a row was appended. The marker id is the dedup key:
    /// the same compaction reaching this client from live, bootstrap, a paged
    /// window, or a legacy import must produce one row, so a second sighting
    /// refreshes the row in place instead of appending. Refreshing matters
    /// because sightings are not equally rich — a paged historical marker can
    /// carry metrics a live one raced past.
    ///
    /// Deliberately touches no message index, no tool index, no typing state,
    /// and no turn state. A marker is not a message and not a turn boundary.
    pub fn push_compaction_marker(
        &self,
        agent_id: AgentId,
        event: ContextCompactionTimelineEvent,
    ) -> bool {
        let marker_id = event.marker_id.clone();
        let existing = self.context_compaction_rows.with_untracked(|map| {
            map.get(&agent_id)
                .and_then(|index| index.get(&marker_id))
                .copied()
        });

        if let Some(row_id) = existing {
            // Write through the row's own signal so the mounted marker
            // re-renders; replacing the enum value would leave the rendered
            // view holding the sighting it mounted with.
            let signal = self.chat_rows.with_untracked(|rows| {
                rows.get(&agent_id)?
                    .iter()
                    .find(|row| row.id == row_id)
                    .and_then(|row| row.compaction_marker().cloned())
            });
            if let Some(signal) = signal {
                signal.update(|existing| merge_richer_marker(existing, event));
            }
            return false;
        }

        let handle = ChatRowHandle::context_compaction(event);
        let row_id = handle.id;
        self.chat_rows.update(|rows| {
            rows.entry(agent_id.clone()).or_default().push(handle);
        });
        self.context_compaction_rows.update(|map| {
            map.entry(agent_id).or_default().insert(marker_id, row_id);
        });
        true
    }

    /// Append a retry/cancel notice as a transcript row.
    ///
    /// No dedup index, unlike compaction markers: a notice has no id and is
    /// never seen twice from different sources. It reaches this client only
    /// through the server's ordered event stream — live, bootstrap replay, or a
    /// strictly-older history page — so stream position is its identity, and a
    /// second `RetryAttempt` is a second attempt rather than a re-sighting.
    pub fn push_chat_notice(&self, agent_id: AgentId, notice: ChatNotice) {
        self.chat_rows.update(|rows| {
            rows.entry(agent_id)
                .or_default()
                .push(ChatRowHandle::notice(notice));
        });
    }

    /// Prepend a history page, funnelling its markers through the same
    /// upsert/dedup rule as live and bootstrap.
    ///
    /// Two dedup axes, and the earlier version only handled one:
    ///
    /// * **Cross-source** — a marker already on screen is merged in place
    ///   rather than prepended again. Merging (not dropping) matters because a
    ///   paged copy can be *richer* than the live one that raced past it.
    /// * **Within the page** — two copies of one marker id inside a single
    ///   returned page both passed the old pre-prepend check, because the index
    ///   was only written after the whole page was inserted. That left two rows
    ///   visible with the index pointing at the second.
    ///
    /// Page order is preserved: the first occurrence keeps its position and
    /// later occurrences merge into it.
    pub fn prepend_history_page(&self, agent_id: &AgentId, rows: Vec<ChatRowHandle>) {
        let mut deduped: Vec<ChatRowHandle> = Vec::with_capacity(rows.len());
        // marker id -> index into `deduped`, for in-page collapsing.
        let mut page_markers: HashMap<CompactionObservationId, usize> = HashMap::new();

        for row in rows {
            let Some(signal) = row.compaction_marker().cloned() else {
                deduped.push(row);
                continue;
            };
            let event = signal.get_untracked();
            let marker_id = event.marker_id.clone();

            // Already on screen from live/bootstrap/an earlier page: merge in
            // place and drop the paged row.
            if let Some(existing_row) = self
                .context_compaction_rows
                .with_untracked(|map| map.get(agent_id).and_then(|i| i.get(&marker_id)).copied())
            {
                let existing_signal = self.chat_rows.with_untracked(|map| {
                    map.get(agent_id)?
                        .iter()
                        .find(|candidate| candidate.id == existing_row)
                        .and_then(|candidate| candidate.compaction_marker().cloned())
                });
                if let Some(existing_signal) = existing_signal {
                    existing_signal.update(|existing| merge_richer_marker(existing, event));
                }
                continue;
            }

            // Seen earlier in this same page: merge into that row.
            if let Some(index) = page_markers.get(&marker_id).copied() {
                if let Some(first) = deduped[index].compaction_marker() {
                    first.update(|existing| merge_richer_marker(existing, event));
                }
                continue;
            }

            page_markers.insert(marker_id, deduped.len());
            deduped.push(row);
        }

        self.prepend_chat_rows(agent_id, deduped);
    }

    /// Prepend already-materialized rows and register any markers among them.
    pub fn prepend_chat_rows(&self, agent_id: &AgentId, rows: Vec<ChatRowHandle>) {
        if rows.is_empty() {
            return;
        }
        let markers: Vec<(CompactionObservationId, ChatRowId)> = rows
            .iter()
            .filter_map(|row| {
                row.compaction_marker()
                    .map(|event| (event.with_untracked(|e| e.marker_id.clone()), row.id))
            })
            .collect();

        self.chat_rows.update(|map| {
            let current = map.remove(agent_id).unwrap_or_default();
            let mut combined = rows;
            combined.extend(current);
            map.insert(agent_id.clone(), combined);
        });

        if !markers.is_empty() {
            self.context_compaction_rows.update(|map| {
                let index = map.entry(agent_id.clone()).or_default();
                for (marker_id, row_id) in markers {
                    index.insert(marker_id, row_id);
                }
            });
        }
    }

    pub fn push_chat_entry(&self, agent_id: AgentId, entry: ChatMessageEntry) -> ChatRowHandle {
        let handle = ChatRowHandle::new(entry);
        let entry_signal = handle
            .message_entry()
            .expect("push_chat_entry builds a message row")
            .clone();
        let (indexed_tool_call_ids, message_id) = entry_signal.with_untracked(|entry| {
            (
                entry
                    .tool_requests
                    .iter()
                    .map(|tool| tool.request.tool_call_id.clone())
                    .collect::<Vec<_>>(),
                entry.message.message_id.clone(),
            )
        });

        self.chat_rows.update(|rows| {
            let agent_rows = rows.entry(agent_id.clone()).or_default();
            agent_rows.push(handle.clone());
        });

        if !indexed_tool_call_ids.is_empty() {
            self.chat_tool_rows.update(|indexes| {
                let agent_index = indexes.entry(agent_id.clone()).or_default();
                for tool_call_id in indexed_tool_call_ids {
                    agent_index.insert(ToolCallId(tool_call_id), handle.id);
                }
            });
        }

        if let Some(message_id) = message_id {
            self.chat_message_rows.update(|indexes| {
                indexes
                    .entry(agent_id)
                    .or_default()
                    .entry(message_id)
                    .or_insert(handle.id);
            });
        }

        handle
    }

    /// Patch the row matching `update.message_id` with whichever of
    /// `model_info` / `token_usage` / `context_breakdown` are `Some`. A
    /// `None` update field means "leave the existing value alone" — this
    /// is a partial update, not a replace. Unknown message ids are
    /// logged and otherwise ignored: server-side guarantees that the
    /// `MessageMetadataUpdated` for a Codex turn arrives after the
    /// visible `StreamEnd` that created the row, but if the row was
    /// dropped (compaction, agent close) by the time the update lands
    /// we just want to no-op, not crash.
    pub fn apply_chat_message_metadata(
        &self,
        agent_id: &AgentId,
        update: MessageMetadataUpdateData,
    ) {
        let row_id = self.chat_message_rows.with_untracked(|indexes| {
            indexes
                .get(agent_id)
                .and_then(|agent_index| agent_index.get(&update.message_id).copied())
        });
        let Some(row_id) = row_id else {
            log::warn!(
                "chat_event message_metadata_updated unknown message_id agent_id={} message_id={}",
                agent_id,
                update.message_id
            );
            return;
        };
        let Some(handle) = self.chat_row_by_id_untracked(agent_id, row_id) else {
            log::warn!(
                "chat_event message_metadata_updated row gone agent_id={} message_id={} row_id={:?}",
                agent_id,
                update.message_id,
                row_id
            );
            return;
        };
        // A marker row can hold the id slot the index pointed at only if the
        // index was corrupted; markers are never registered there. Skipping
        // rather than panicking keeps a metadata update from taking the tab
        // down.
        let Some(entry_signal) = handle.message_entry().cloned() else {
            log::warn!(
                "chat_event message_metadata_updated targeted a non-message row agent_id={agent_id} row_id={row_id:?}"
            );
            return;
        };
        let row_message_id = entry_signal.with_untracked(|entry| entry.message.message_id.clone());
        if row_message_id.as_ref() != Some(&update.message_id) {
            log::warn!(
                "chat_event message_metadata_updated stale row agent_id={} expected_message_id={} row_message_id={:?} row_id={:?}",
                agent_id,
                update.message_id,
                row_message_id,
                row_id
            );
            return;
        }
        entry_signal.update(|entry| {
            if let Some(model_info) = update.model_info {
                entry.message.model_info = Some(model_info);
            }
            if let Some(token_usage) = update.token_usage {
                entry.message.token_usage = Some(token_usage);
            }
            if let Some(context_breakdown) = update.context_breakdown {
                entry.message.context_breakdown = Some(context_breakdown);
            }
        });
    }

    /// The newest row that can actually hold a message or a tool card.
    ///
    /// Tool-card attachment falls back to "the last row" when no row claims
    /// the call. Once markers exist, the last row may be a marker, which has
    /// no message to attach to — the card would be silently dropped. This
    /// skips past markers to the newest real message row.
    pub fn last_chat_message_row_untracked(&self, agent_id: &AgentId) -> Option<ChatRowHandle> {
        self.chat_rows.with_untracked(|rows| {
            rows.get(agent_id)?
                .iter()
                .rev()
                .find(|row| row.message_entry().is_some())
                .cloned()
        })
    }

    pub fn chat_row_by_id_untracked(
        &self,
        agent_id: &AgentId,
        row_id: ChatRowId,
    ) -> Option<ChatRowHandle> {
        self.chat_rows.with_untracked(|rows| {
            rows.get(agent_id)
                .and_then(|rows| rows.iter().find(|row| row.id == row_id).cloned())
        })
    }

    pub fn index_chat_tool_row(&self, agent_id: &AgentId, tool_call_id: String, row_id: ChatRowId) {
        self.chat_tool_rows.update(|indexes| {
            indexes
                .entry(agent_id.clone())
                .or_default()
                .insert(ToolCallId(tool_call_id), row_id);
        });
    }

    pub fn chat_row_for_tool_untracked(
        &self,
        agent_id: &AgentId,
        tool_call_id: &str,
    ) -> Option<ChatRowHandle> {
        let row_id = self.chat_tool_rows.with_untracked(|indexes| {
            indexes.get(agent_id).and_then(|agent_index| {
                agent_index
                    .get(&ToolCallId(tool_call_id.to_owned()))
                    .copied()
            })
        })?;
        self.chat_row_by_id_untracked(agent_id, row_id)
    }

    pub fn chat_row_for_tool(
        &self,
        agent_id: &AgentId,
        tool_call_id: &str,
    ) -> Option<ChatRowHandle> {
        let row_id = self.chat_tool_rows.with(|indexes| {
            indexes.get(agent_id).and_then(|agent_index| {
                agent_index
                    .get(&ToolCallId(tool_call_id.to_owned()))
                    .copied()
            })
        })?;
        self.chat_rows.with(|rows| {
            rows.get(agent_id)
                .and_then(|rows| rows.iter().find(|row| row.id == row_id).cloned())
        })
    }

    pub fn selected_host_settings(&self) -> Option<HostSettings> {
        let host_id = self.selected_host_id.get()?;
        self.host_settings_by_host.get().get(&host_id).cloned()
    }

    pub fn selected_host_settings_untracked(&self) -> Option<HostSettings> {
        let host_id = self.selected_host_id.get_untracked()?;
        self.host_settings_by_host
            .get_untracked()
            .get(&host_id)
            .cloned()
    }

    pub fn selected_host_settings_schema(&self) -> Option<serde_json::Value> {
        let host_id = self.selected_host_id.get()?;
        self.host_settings_schema_by_host
            .get()
            .get(&host_id)
            .cloned()
    }

    pub fn selected_host_configured_secrets_untracked(&self) -> Vec<protocol::ConfiguredSecret> {
        let Some(host_id) = self.selected_host_id.get_untracked() else {
            return Vec::new();
        };
        self.configured_secrets_by_host
            .get_untracked()
            .get(&host_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn selected_host_backend_setup(&self) -> Option<Vec<BackendSetupInfo>> {
        let host_id = self.selected_host_id.get()?;
        self.backend_setup_by_host.get().get(&host_id).cloned()
    }

    pub fn selected_host_connection_status(&self) -> ConnectionStatus {
        let Some(host_id) = self.selected_host_id.get() else {
            return ConnectionStatus::Disconnected;
        };
        self.connection_statuses
            .get()
            .get(&host_id)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    pub fn selected_host_command_error(&self) -> Option<String> {
        let host_id = self.selected_host_id.get()?;
        self.command_errors_by_host.get().get(&host_id).cloned()
    }

    /// Apply a server-emitted Add-report shuffle suggestion notify. Each
    /// notify bumps a per-(host, team) serial so the open dialog can
    /// detect fresh suggestions without re-applying stale ones.
    pub fn record_team_member_shuffle_suggestion(
        &self,
        host_id: &str,
        payload: TeamMemberShuffleSuggestionNotifyPayload,
    ) {
        let TeamMemberShuffleSuggestionNotifyPayload {
            team_id,
            suggestion,
        } = payload;
        self.team_member_shuffle_suggestions.update(|map| {
            let host_map = map.entry(host_id.to_owned()).or_default();
            let previous_serial = host_map
                .get(&team_id)
                .map(|entry| entry.serial)
                .unwrap_or(0);
            host_map.insert(
                team_id,
                TeamMemberShuffleSuggestionEntry {
                    suggestion,
                    serial: previous_serial.saturating_add(1),
                },
            );
        });
    }

    pub fn active_project_ref_untracked(&self) -> Option<ActiveProjectRef> {
        self.active_project.get_untracked()
    }

    pub fn queue_pending_agent_session_settings(
        &self,
        host_id: String,
        project_id: Option<ProjectId>,
        values: SessionSettingsValues,
    ) -> u64 {
        let id = self.next_pending_agent_session_settings_id.get_untracked();
        self.next_pending_agent_session_settings_id
            .set(id.wrapping_add(1));
        self.pending_agent_session_settings.update(|pending| {
            pending
                .entry((host_id, project_id))
                .or_default()
                .push_back(PendingAgentSessionSettings { id, values });
        });
        id
    }

    pub fn discard_pending_agent_session_settings(
        &self,
        host_id: &str,
        project_id: Option<&ProjectId>,
        id: u64,
    ) {
        let key = (host_id.to_owned(), project_id.cloned());
        self.pending_agent_session_settings.update(|pending| {
            let remove_key = if let Some(queue) = pending.get_mut(&key) {
                queue.retain(|entry| entry.id != id);
                queue.is_empty()
            } else {
                false
            };
            if remove_key {
                pending.remove(&key);
            }
        });
    }

    pub fn take_pending_agent_session_settings(
        &self,
        host_id: &str,
        project_id: Option<&ProjectId>,
    ) -> Option<SessionSettingsValues> {
        let key = (host_id.to_owned(), project_id.cloned());
        self.pending_agent_session_settings
            .try_update(|pending| {
                let queue = pending.get_mut(&key)?;
                let entry = queue.pop_front()?;
                if queue.is_empty() {
                    pending.remove(&key);
                }
                Some(entry.values)
            })
            .flatten()
    }

    pub fn restore_active_project_after_host_bootstrap(&self, host_id: &str) {
        let Some(pending) = self
            .pending_active_project_restore
            .get_untracked()
            .filter(|pending| pending.host_id == host_id)
        else {
            return;
        };
        self.pending_active_project_restore.set(None);

        if self.active_project.get_untracked().is_some() {
            return;
        }
        let exists = self.projects.with_untracked(|projects| {
            projects.iter().any(|project| {
                project.host_id == pending.host_id && project.project.id == pending.project_id
            })
        });
        if exists {
            // Restoring, not choosing: must not retire the chat/draft that
            // are about to be restored on top of this project.
            self.activate_restored_project(Some(pending));
        } else {
            persist_active_project(None);
        }
    }

    /// Every queued pending session-settings value, for tests that need to
    /// inspect what a spawn actually queues rather than trusting the draft
    /// signals it was built from.
    ///
    /// The queue is a separate channel from the `SpawnAgent` payload: settings
    /// are stored under the target host and project whether or not they also
    /// travel in the frame, and they do not when a launch profile is deferred
    /// to. Gated to wasm because only a test that drives the real
    /// `spawn_new_chat` populates it, and that spawn ends in `spawn_local`.
    #[cfg(all(test, target_arch = "wasm32"))]
    pub(crate) fn pending_settings_values_for_tests(&self) -> Vec<SessionSettingsValues> {
        self.pending_agent_session_settings
            .get_untracked()
            .values()
            .flat_map(|queue| queue.iter().map(|pending| pending.values.clone()))
            .collect()
    }

    /// The composer belonging to one specific chat tab, created on first use.
    ///
    /// Keyed by tab rather than by agent so a "New Chat" tab (no agent yet) has
    /// somewhere to keep its draft, and so a tab whose agent is swapped
    /// underneath it keeps the text the user already typed.
    pub fn composer_for(&self, tab: TabId) -> ComposerHandle {
        let mut composers = self
            .composers
            .lock()
            .expect("composer registry is only held long enough to clone a handle");
        composers.entry(tab).or_default().clone()
    }

    /// The composer of the chat the user is currently working in — the focused
    /// pane's chat, else the other pane's. Tracks `center_zone`, so a caller
    /// inside an effect re-runs when the owner changes.
    ///
    /// Every chat renders its own composer; this accessor exists for paths with
    /// no pane of their own (keyboard commands, the dispatcher, launch menus)
    /// that must act on the one the user is actually looking at.
    ///
    /// Browser-only: the tracked form is consumed by the persistence effects,
    /// which only exist in the browser. Off-wasm callers use the untracked
    /// accessor below.
    #[cfg(target_arch = "wasm32")]
    pub fn composer(&self) -> ComposerHandle {
        match self.center_zone.with(CenterZoneState::composer_owner) {
            Some((_, tab)) => self.composer_for(tab),
            None => self.detached_composer.clone(),
        }
    }

    pub fn composer_untracked(&self) -> ComposerHandle {
        match self
            .center_zone
            .with_untracked(CenterZoneState::composer_owner)
        {
            Some((_, tab)) => self.composer_for(tab),
            None => self.detached_composer.clone(),
        }
    }

    /// Every chat tab currently visible — at most one per pane. These are the
    /// tabs that have a mounted composer, so they are the ones whose drafts are
    /// reconciled and checkpointed.
    #[cfg(target_arch = "wasm32")]
    fn visible_chat_tabs(&self) -> Vec<TabId> {
        self.center_zone.with_untracked(|center_zone| {
            center_zone
                .panes()
                .filter_map(|(_, pane)| {
                    let tab_id = pane.active_tab_id?;
                    let tab = pane.tabs.iter().find(|tab| tab.id == tab_id)?;
                    matches!(&tab.content, TabContent::Chat { .. }).then_some(tab_id)
                })
                .collect()
        })
    }

    /// Every composer that could be holding unsent text, visible or not,
    /// ordered so the visible one is **last**.
    ///
    /// Order is load-bearing on the flush path. Two "New Chat" tabs in the same
    /// project derive the same `PersistedComposerDraftOwner::NewChat` identity,
    /// so they share one store entry; checkpointing the visible composer last
    /// makes the chat the user was actually typing in the one that survives.
    #[cfg(target_arch = "wasm32")]
    fn composers_for_flush(&self) -> Vec<ComposerHandle> {
        let visible = self
            .center_zone
            .with_untracked(CenterZoneState::composer_owner)
            .map(|(_, tab)| tab);
        let mut handles: Vec<ComposerHandle> = {
            let composers = self
                .composers
                .lock()
                .expect("composer registry is only held long enough to clone a handle");
            composers
                .iter()
                .filter(|(tab, _)| Some(**tab) != visible)
                .map(|(_, composer)| composer.clone())
                .collect()
        };
        handles.push(match visible {
            Some(tab) => self.composer_for(tab),
            None => self.detached_composer.clone(),
        });
        handles
    }

    /// Drop composers whose tabs are gone.
    fn forget_composers(&self, doomed: &HashSet<TabId>) {
        let mut composers = self
            .composers
            .lock()
            .expect("composer registry is only held long enough to clone a handle");
        composers.retain(|id, _| !doomed.contains(id));
    }

    /// Reset every spawn choice on the visible composer.
    ///
    /// The selection is **one** host-bound value, not a bag of independent
    /// ones: the backend, launch profile, custom agent, and profile-derived
    /// session settings are all chosen against a single host and are all
    /// meaningless against another. Clearing only the obvious three left a
    /// custom-agent id and provider-specific settings armed, which the spawn
    /// path then read and sent to whichever host it was targeting.
    ///
    /// Every mismatch path routes through here so the set can never drift
    /// apart again.
    pub(crate) fn clear_draft_selection(&self) {
        self.composer_untracked().clear_selection();
    }

    /// The selection as it should currently be stored.
    ///
    /// Each half falls back to its *pending* form when it has not been applied
    /// yet, so writing storage for one reason never discards the other's intent
    /// — a chat that fails to restore must not take a still-valid profile
    /// choice with it.
    fn current_selection_snapshot(&self) -> PersistedWorkspaceSelection {
        let live_chat = self.active_agent.get_untracked().and_then(|agent_ref| {
            self.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|agent| {
                        agent.host_id == agent_ref.host_id && agent.agent_id == agent_ref.agent_id
                    })
                    .map(|agent| PersistedChatRef {
                        host_id: agent_ref.host_id.clone(),
                        agent_id: agent_ref.agent_id.clone(),
                        project_id: agent.project_id.clone(),
                        session_id: agent.session_id.clone(),
                    })
            })
        });
        // Only the visible composer's selection is persisted. Restoring is
        // single-valued (one `PersistedDraftSelection`), so persisting every
        // open draft would make the last writer win at an arbitrary moment;
        // the chat the user is actually working in is the honest choice.
        let composer = self.composer_untracked();
        let live_draft = composer
            .selection_host
            .get_untracked()
            .map(|host_id| PersistedDraftSelection {
                host_id,
                backend: composer.backend_override.get_untracked(),
                launch_profile: composer.launch_profile_id.get_untracked(),
            })
            .filter(|draft| !draft.is_empty());
        PersistedWorkspaceSelection {
            active_chat: live_chat.or_else(|| self.pending_active_chat_restore.get_untracked()),
            draft: live_draft.or_else(|| self.pending_draft_restore.get_untracked()),
        }
    }

    /// Register the reactive wiring that keeps the persisted selection and the
    /// draft host binding in step with what the user does.
    ///
    /// Deliberately **not** in `AppState::new`. A constructor that registers
    /// effects can only be called where a reactive executor is already running:
    /// native tests have none at all, and a wasm test that builds an
    /// `AppState` without mounting does not either. Effects belong to the app
    /// root, which mounts and therefore has one; construction stays inert and
    /// callable from anywhere.
    ///
    /// Browser-only in substance — persistence writes `localStorage`, and the
    /// host guard reacts to navigation. Both have synchronous counterparts on
    /// the paths that matter off the browser.
    #[cfg(target_arch = "wasm32")]
    pub fn install_browser_effects(&self) {
        self.composer_drafts.set(load_composer_drafts());

        // Both composer effects re-read the visible tab set on every run, so
        // the dynamic set of per-pane signals they depend on is re-collected
        // each time rather than fixed at install.
        let composer_owner_state = self.clone();
        Effect::new(move |previous: Option<Vec<TabId>>| {
            composer_owner_state.center_zone.with(|_| ());
            composer_owner_state.active_project.with(|_| ());
            composer_owner_state.selected_host_id.with(|_| ());
            composer_owner_state.active_agent.with(|_| ());
            composer_owner_state.agents.with(|_| ());
            let visible = composer_owner_state.visible_chat_tabs();
            // A chat leaving view is the moment its draft must reach storage.
            // Per-tab composers keep their own text, so the reconcile below no
            // longer sees an owner change on the way out and would otherwise
            // leave the outgoing draft sitting in the debounce queue.
            if previous.is_some_and(|previous| previous != visible) {
                composer_owner_state.flush_composer_drafts();
            }
            for tab in &visible {
                composer_owner_state.reconcile_composer_draft_owner_for_tab(*tab);
            }
            visible
        });

        let composer_text_state = self.clone();
        Effect::new(move |_| {
            composer_text_state.center_zone.with(|_| ());
            let mut dirty = false;
            for tab in composer_text_state.visible_chat_tabs() {
                let composer = composer_text_state.composer_for(tab);
                composer.text.with(|_| ());
                composer.draft_owner.with(|_| ());
                dirty |= composer_text_state.checkpoint_composer_draft(&composer);
            }
            if dirty {
                composer_text_state.schedule_composer_draft_persist();
            }
        });

        let selection_state = self.clone();
        Effect::new(move |_| {
            // Tracked reads: the stored identity must follow a session
            // assignment that arrives after the tab is already open.
            let _ = selection_state.active_agent.get();
            // `composer()` tracks `center_zone`, so switching chats re-runs
            // this and persists the newly visible composer's selection.
            let composer = selection_state.composer();
            let _ = composer.backend_override.get();
            let _ = composer.launch_profile_id.get();
            let _ = composer.selection_host.get();
            let _ = selection_state.agents.get();
            selection_state.persist_selection_snapshot_if_settled();
        });

        let draft_host_state = self.clone();
        Effect::new(move |_| {
            // Tracked so a context change through *any* route — including
            // selecting a chat tab on another host, which no synchronous call
            // site covers — drops a foreign draft.
            let _ = draft_host_state.chat_context_host_id();
            draft_host_state.drop_draft_bound_to_another_host();
        });
    }

    /// The conversation identity a given chat tab's draft belongs to.
    ///
    /// Derived from the tab rather than from `composer_owner()` so each pane's
    /// composer files its draft under its own chat.
    #[cfg(target_arch = "wasm32")]
    fn composer_draft_owner_for_tab(&self, tab_id: TabId) -> Option<PersistedComposerDraftOwner> {
        let content = self
            .center_zone
            .with_untracked(|center_zone| center_zone.tab(tab_id).map(|tab| tab.content.clone()))?;
        match content {
            TabContent::Chat {
                agent_ref: Some(agent_ref),
                ..
            } => {
                let agent = self.agents.with_untracked(|agents| {
                    agents
                        .iter()
                        .find(|agent| {
                            agent.host_id == agent_ref.host_id
                                && agent.agent_id == agent_ref.agent_id
                        })
                        .cloned()
                })?;
                Some(PersistedComposerDraftOwner::ActiveChat {
                    host_id: agent_ref.host_id,
                    agent_id: agent_ref.agent_id,
                    project_id: agent.project_id,
                    session_id: agent.session_id,
                })
            }
            TabContent::Chat {
                pending_team_member: Some(pending),
                ..
            } => Some(PersistedComposerDraftOwner::TeamMember {
                project_id: self
                    .active_project
                    .get_untracked()
                    .filter(|project| project.host_id == pending.host_id)
                    .map(|project| project.project_id),
                host_id: pending.host_id,
                member_id: pending.member_id,
            }),
            TabContent::Chat { .. } => {
                let host_id = self.chat_context_host_id_untracked()?;
                let project_id = self
                    .active_project
                    .get_untracked()
                    .filter(|project| project.host_id == host_id)
                    .map(|project| project.project_id);
                Some(PersistedComposerDraftOwner::NewChat {
                    host_id,
                    project_id,
                })
            }
            _ => None,
        }
    }

    /// Re-file one chat's draft when the conversation under it changes.
    ///
    /// Scoped to a single tab: a pane whose New Chat upgrades to a live agent,
    /// or whose agent is retargeted by compaction, re-files its own draft
    /// without touching the other pane's composer.
    #[cfg(target_arch = "wasm32")]
    fn reconcile_composer_draft_owner_for_tab(&self, tab_id: TabId) {
        let composer = self.composer_for(tab_id);
        let next = self.composer_draft_owner_for_tab(tab_id);
        let current = composer.draft_owner.get_untracked();
        if current == next {
            return;
        }

        if let (Some(current), Some(next)) = (&current, &next)
            && current.same_conversation(next)
        {
            composer.draft_owner.set(Some(next.clone()));
            self.composer_drafts.update(|drafts| {
                drafts.restore(next);
            });
            self.flush_composer_drafts();
            self.restore_composer_draft(&composer, Some(next));
            return;
        }

        let text = composer.text.get_untracked();
        if current.is_none() && next.is_some() && !text.is_empty() {
            composer.draft_owner.set(next.clone());
            if self.checkpoint_composer_draft(&composer) {
                self.schedule_composer_draft_persist();
            }
            return;
        }

        self.flush_composer_drafts();
        composer.text.set(String::new());
        composer.draft_owner.set(next.clone());
        self.restore_composer_draft(&composer, next.as_ref());
    }

    #[cfg(target_arch = "wasm32")]
    fn restore_composer_draft(
        &self,
        composer: &ComposerHandle,
        owner: Option<&PersistedComposerDraftOwner>,
    ) {
        if !composer.text.get_untracked().is_empty() {
            return;
        }
        let Some(owner) = owner else {
            return;
        };
        let restored = self
            .composer_drafts
            .try_update(|drafts| drafts.restore(owner))
            .flatten();
        if let Some(text) = restored {
            composer.text.set(text);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn checkpoint_composer_draft(&self, composer: &ComposerHandle) -> bool {
        let Some(owner) = composer.draft_owner.get_untracked() else {
            return false;
        };
        let text = composer.text.get_untracked();
        if text.is_empty() {
            return self
                .composer_drafts
                .try_update(|drafts| drafts.remove(&owner))
                .unwrap_or(false);
        }

        let update = self
            .composer_drafts
            .try_update(|drafts| drafts.upsert(owner, text))
            .unwrap_or(DraftStoreUpdate::EntryTooLarge);
        match update {
            DraftStoreUpdate::Stored { evicted } => {
                if evicted > 0 {
                    log::warn!("composer draft bounds queued {evicted} evictions");
                    self.composer_draft_pending_evictions
                        .update(|pending| *pending = pending.saturating_add(evicted));
                }
                true
            }
            DraftStoreUpdate::EntryTooLarge => {
                log::warn!("composer draft exceeds the per-entry persistence limit");
                notify_composer_draft_limit(self.composer_draft_limit_notified);
                true
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn cancel_composer_draft_persist(&self) {
        let scheduler_id = self.composer_draft_persistence.id;
        let timeout =
            COMPOSER_DRAFT_TIMEOUTS.with(|timeouts| timeouts.borrow_mut().remove(&scheduler_id));
        if let Some(timeout) = timeout {
            COMPOSER_DRAFT_SCHEDULERS.with(|schedulers| {
                if let Some(scheduler) = schedulers.borrow().get(&scheduler_id) {
                    (scheduler.cancel)(timeout.handle);
                }
            });
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn schedule_composer_draft_persist(&self) {
        use wasm_bindgen::JsCast;

        self.cancel_composer_draft_persist();
        let drafts = self.composer_drafts;
        // Resolved when the debounce fires, not now: the protected-from-
        // eviction entry is whichever chat the user is in at write time.
        let owner_state = self.clone();
        let pending_evictions = self.composer_draft_pending_evictions;
        let eviction_notified = self.composer_draft_eviction_notified;
        let limit_notified = self.composer_draft_limit_notified;
        let failure_notified = self.composer_draft_persistence_failure_notified;
        let scheduler_id = self.composer_draft_persistence.id;
        let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            let active_owner = owner_state.composer_untracked().draft_owner.get_untracked();
            let outcome = drafts
                .try_update(|drafts| persist_composer_drafts(drafts, active_owner.as_ref()))
                .unwrap_or_default();
            surface_composer_draft_persistence_outcome(
                pending_evictions,
                eviction_notified,
                limit_notified,
                failure_notified,
                outcome,
            );
            COMPOSER_DRAFT_TIMEOUTS.with(|timeouts| {
                timeouts.borrow_mut().remove(&scheduler_id);
            });
        });
        let scheduler = COMPOSER_DRAFT_SCHEDULERS.with(|schedulers| {
            schedulers
                .borrow()
                .get(&scheduler_id)
                .cloned()
                .unwrap_or_default()
        });
        match (scheduler.schedule)(
            callback.as_ref().unchecked_ref(),
            COMPOSER_DRAFT_DEBOUNCE_MS,
        ) {
            Ok(handle) => {
                COMPOSER_DRAFT_TIMEOUTS.with(|timeouts| {
                    timeouts.borrow_mut().insert(
                        scheduler_id,
                        ComposerDraftTimeout {
                            handle,
                            _callback: callback,
                        },
                    );
                });
            }
            Err(error) => {
                log::warn!("failed to schedule composer draft persistence: {error:?}");
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn flush_composer_drafts(&self) {
        // Every composer is checkpointed, not just the visible one: a flush
        // runs on page hide, and unsent text in the other pane is exactly what
        // this exists to rescue.
        for composer in self.composers_for_flush() {
            self.checkpoint_composer_draft(&composer);
        }
        self.cancel_composer_draft_persist();
        let active_owner = self.composer_untracked().draft_owner.get_untracked();
        let outcome = self
            .composer_drafts
            .try_update(|drafts| persist_composer_drafts(drafts, active_owner.as_ref()))
            .unwrap_or_default();
        surface_composer_draft_persistence_outcome(
            self.composer_draft_pending_evictions,
            self.composer_draft_eviction_notified,
            self.composer_draft_limit_notified,
            self.composer_draft_persistence_failure_notified,
            outcome,
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn flush_composer_drafts(&self) {}

    /// Persist the current selection, unless a restore has not yet resolved.
    ///
    /// At cold start `active_agent` is `None` because no tab has been restored
    /// yet, and writing that would erase the very pointer the restore is about
    /// to read. A pending restore is retired explicitly by
    /// `retire_pending_restores`, which every deliberate center/project
    /// selection calls — including the ones (New Chat, Home, an unvisited
    /// project) that legitimately leave `active_agent` at `None`.
    ///
    /// Gated to wasm because this gate exists solely for the persistence
    /// effect, which is itself browser-only. The native paths that write
    /// storage — the retire and restore paths — call
    /// [`Self::persist_selection_snapshot`] directly, because each already
    /// knows whether its own intent is settled and has no pending state to
    /// test. Unlike [`Self::drop_draft_bound_to_another_host`], which the
    /// effect shares with a synchronous caller, this has no native counterpart
    /// to keep.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn persist_selection_snapshot_if_settled(&self) {
        let restore_pending = self
            .pending_active_chat_restore
            .with_untracked(|pending| pending.is_some())
            || self
                .pending_draft_restore
                .with_untracked(|pending| pending.is_some());
        if restore_pending {
            return;
        }
        self.persist_selection_snapshot();
    }

    /// Drop a live draft selection whose host is no longer the chat context.
    ///
    /// The selection is only meaningful against the host it was made for: a
    /// launch-profile id means nothing in another host's catalog, and a backend
    /// enabled on one host may be disabled on the next. This is what keeps the
    /// binding holding *after* a successful restore, not only while it is
    /// pending.
    ///
    /// Scoped to the visible composer. A background chat's selection is bound
    /// to *its own* host and stays valid there; it is re-checked when the user
    /// switches back to it, because that switch re-runs the effect this hangs
    /// off.
    pub(crate) fn drop_draft_bound_to_another_host(&self) {
        let Some(bound_host) = self.composer_untracked().selection_host.get_untracked() else {
            return;
        };
        // No chat context at all is not a *different* host. On a cold start
        // with no project selected and no chat open there is nothing to
        // contradict the binding, and clearing here wiped a draft the restore
        // had just legitimately applied.
        let Some(context_host) = self.chat_context_host_id_untracked() else {
            return;
        };
        if context_host == bound_host {
            return;
        }
        self.clear_draft_selection();
    }

    /// Write the current selection to storage immediately.
    ///
    /// The persistence effect only fires on a reactive change, and a restore
    /// that *fails* changes nothing — so without this an unresolvable pointer
    /// would be retried on every reload forever.
    pub(crate) fn persist_selection_snapshot(&self) {
        persist_workspace_selection(&self.current_selection_snapshot());
    }

    /// Forget a chat pointer that could not be restored, leaving any draft
    /// intent alone.
    pub(crate) fn retire_pending_chat_restore(&self) {
        self.pending_active_chat_restore.set(None);
        let mut snapshot = self.current_selection_snapshot();
        snapshot.active_chat = None;
        persist_workspace_selection(&snapshot);
    }

    /// Forget a draft selection that could not be restored, leaving any chat
    /// intent alone.
    pub(crate) fn retire_pending_draft_restore(&self) {
        self.pending_draft_restore.set(None);
        let mut snapshot = self.current_selection_snapshot();
        snapshot.draft = None;
        persist_workspace_selection(&snapshot);
    }

    /// Abandon any pending restore because the user has just chosen what the
    /// center should show.
    ///
    /// Every deliberate selection must call this, **including** the ones that
    /// leave `active_agent` at `None` — New Chat, Home, and switching to a
    /// project with no view memory all open an empty draft. Inferring intent
    /// from `active_agent: Some` alone would let a late bootstrap insert the
    /// old chat over a choice the user had already made, which is both a focus
    /// steal and a project/backend identity contradiction.
    pub fn retire_pending_restores(&self) {
        let had_chat = self
            .pending_active_chat_restore
            .try_update(|pending| pending.take())
            .flatten()
            .is_some();
        let had_draft = self
            .pending_draft_restore
            .try_update(|pending| pending.take())
            .flatten()
            .is_some();
        if !had_chat && !had_draft {
            return;
        }
        // The effect only writes once nothing is pending, and it may not re-run
        // for a change that happened before this call. Write the retirement
        // through immediately so the abandoned intent cannot come back.
        self.persist_selection_snapshot();
    }

    /// Whether the project at `(host_id, project_id)` accepts ProjectAddRoot /
    /// ProjectDeleteRoot. Per §6.5/§6.6 of the workbenches design doc:
    ///
    /// - A workbench's roots are managed only by WorkbenchCreate /
    ///   WorkbenchRemove — root edits are rejected with `InvalidInput`.
    /// - A standalone parent that has at least one workbench child is
    ///   rejected with `Conflict` because root edits would break the
    ///   parent_root linkage in every child workbench.
    /// - Otherwise (standalone with no children), root edits are allowed.
    ///
    /// The UI mirrors this: hide / disable add-root and per-root remove
    /// affordances when the answer is `false`. The server is still the
    /// enforcement boundary; this is just a projection of state.
    pub fn can_manage_project_roots(&self, host_id: &str, project_id: &ProjectId) -> bool {
        let projects = self.projects.get();
        let Some(project) = projects
            .iter()
            .find(|info| info.host_id == host_id && &info.project.id == project_id)
        else {
            return false;
        };
        if project.project.is_workbench() {
            return false;
        }
        let has_workbench_children = projects.iter().any(|info| {
            info.host_id == host_id && info.project.parent_project_id() == Some(project_id)
        });
        !has_workbench_children
    }

    /// Change which project the center zone is viewing. Snapshots the outgoing
    /// project's center-zone state into `project_view_memory` and restores the
    /// incoming project's last snapshot (or a fresh empty Chat view for a
    /// project seen for the first time, or Home view when switching to none).
    pub fn switch_active_project(&self, next: Option<ActiveProjectRef>) {
        // A user picking a project is a statement about what the center should
        // show, so it outranks any restore still in flight.
        self.retire_pending_restores();
        self.apply_active_project(next);
    }

    /// Activate a project as part of restoring a reload, **without** retiring
    /// the pending chat/draft.
    ///
    /// Bootstrap restores the project first, then the chat and draft that
    /// depend on it. Routing that first step through the user-selection path
    /// made it consume the very intent the next two steps were about to read,
    /// so an ordinary cold restore silently dropped both. Reactivating a
    /// project the user already had open is not a new choice and must not be
    /// treated as one.
    pub(crate) fn activate_restored_project(&self, next: Option<ActiveProjectRef>) {
        self.apply_active_project(next);
    }

    fn apply_active_project(&self, next: Option<ActiveProjectRef>) {
        let current = self.active_project.get_untracked();
        if current == next {
            return;
        }

        if let Some(outgoing) = current {
            let snapshot = ProjectViewMemory {
                center_zone: Some(self.center_zone.get_untracked()),
                active_terminal: self.active_terminal.get_untracked(),
                open_files: self.open_files.get_untracked(),
                diff_contents: self.diff_contents.get_untracked(),
            };
            self.project_view_memory.update(|map| {
                map.insert(outgoing, snapshot);
            });
        }

        let restored = next.as_ref().and_then(|r| {
            self.project_view_memory
                .with_untracked(|m| m.get(r).cloned())
        });

        self.active_project.set(next.clone());
        persist_active_project(next.as_ref());

        // Notify the host that this project became active so the server can warm
        // code intelligence and restore recent history. This is the one central
        // switch path, so every selection route (rail click, resume, team open,
        // new-chat prefill) is covered. Home/None never notifies. Duplicate
        // sends on switch are fine — the server owns idempotency.
        #[cfg(target_arch = "wasm32")]
        if let Some(accessed) = next.as_ref() {
            let host_id = accessed.host_id.clone();
            let stream = StreamPath(format!("/project/{}", accessed.project_id.0));
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = crate::send::project_accessed(&host_id, stream).await {
                    log::error!("failed to send ProjectAccessed: {error}");
                }
            });
        }

        // active_agent is a Memo derived from center_zone — restoring center_zone
        // implicitly restores it. Tab LRU is reset and re-seeded with the
        // incoming project's active tab so the first render after switch
        // mounts content (avoids a one-frame empty flash before the Effect
        // in `App` fires).
        self.references_state
            .set(ProjectReferencesUiState::default());
        self.code_intel_hover.set(None);
        self.code_intel_navigate_ctx.set(None);
        self.pending_file_opens.set(HashMap::new());

        match (next.is_some(), restored) {
            (true, Some(memory)) => {
                let cz = memory.center_zone.unwrap_or_default();
                self.tab_lru.set(
                    cz.panes()
                        .filter_map(|(_, pane)| pane.active_tab_id)
                        .collect(),
                );
                if let Some(ratio) = cz.split_ratio() {
                    self.center_split_ratio.set(ratio);
                }
                self.center_zone.set(cz);
                self.active_terminal.set(memory.active_terminal);
                self.open_files.set(memory.open_files);
                self.diff_contents.set(memory.diff_contents);
            }
            (true, None) => {
                let mut cz = CenterZoneState::default();
                cz.open(TabContent::empty_chat(), "New Chat".to_string(), true);
                self.tab_lru.set(
                    cz.panes()
                        .filter_map(|(_, pane)| pane.active_tab_id)
                        .collect(),
                );
                self.center_zone.set(cz);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
            (false, _) => {
                let cz = CenterZoneState::default();
                self.tab_lru.set(
                    cz.panes()
                        .filter_map(|(_, pane)| pane.active_tab_id)
                        .collect(),
                );
                self.center_zone.set(cz);
                self.active_terminal.set(None);
                self.open_files.set(HashMap::new());
                self.diff_contents.set(HashMap::new());
            }
        }

        // Synchronous counterpart to the browser-only guard: a project switch is
        // the context change a native build can drive, and a draft bound to the
        // old host must not survive it. Last, so the context it compares
        // against is the settled one — `active_agent` derives from the center
        // zone, which the match above has just replaced.
        self.drop_draft_bound_to_another_host();
    }

    pub fn forget_project_view_memory(&self, project: &ActiveProjectRef) {
        let mut removed_tab_ids = HashSet::new();
        self.project_view_memory.update(|map| {
            if let Some(memory) = map.remove(project)
                && let Some(center_zone) = memory.center_zone
            {
                removed_tab_ids.extend(center_zone.all_tab_ids());
            }
        });
        self.forget_removed_tab_occurrence_state(&removed_tab_ids);
    }

    pub fn active_project_info_untracked(&self) -> Option<ProjectInfo> {
        let active = self.active_project.get_untracked()?;
        self.projects.get_untracked().into_iter().find(|project| {
            project.host_id == active.host_id && project.project.id == active.project_id
        })
    }

    pub fn active_connection_count(&self) -> usize {
        self.connection_statuses
            .get()
            .values()
            .filter(|status| matches!(status, ConnectionStatus::Connected))
            .count()
    }

    pub fn total_host_count(&self) -> usize {
        self.configured_hosts.get().len()
    }

    pub fn clear_host_runtime(&self, host_id: &str) {
        // Paging metadata describes one subscriber snapshot. Carrying it across
        // a disconnect would leave History offering a continuation cursor from
        // a generation the new connection knows nothing about.
        self.session_list_pages
            .update(|pages| pages.retain(|(page_host, _), _| page_host != host_id));
        self.session_list_refresh_in_flight.update(|hosts| {
            hosts.remove(host_id);
        });
        self.pending_agent_session_settings
            .update(|pending| pending.retain(|(pending_host, _), _| pending_host != host_id));
        let host_project_ids: HashSet<ProjectId> = self.projects.with_untracked(|projects| {
            projects
                .iter()
                .filter(|project| project.host_id == host_id)
                .map(|project| project.project.id.clone())
                .collect()
        });
        let active_project_on_host = self
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id);
        if active_project_on_host {
            self.switch_active_project(None);
        }

        let reviews_before = self.reviews.with_untracked(|m| m.len());
        let action_gates_before = self.review_action_pending.with_untracked(|m| m.len());
        let target_gates_before = self
            .review_action_target_pending
            .with_untracked(|s| s.len());
        let create_pending_before = self
            .review_create_pending
            .with_untracked(|m| m.iter().filter(|((h, _), _)| h == host_id).count());
        log::info!(
            "host.clear_host_runtime.start host={host_id} host_projects={} reviews_before={reviews_before} action_gates_before={action_gates_before} target_gates_before={target_gates_before} host_create_pending={create_pending_before}",
            host_project_ids.len()
        );
        // Drop chat-related per-agent state for every agent on this host before
        // we forget the agent list itself. Without this, a reconnect re-replays
        // every event and the dispatcher appends duplicate messages onto the
        // already-cached vectors.
        let agent_ids: Vec<AgentId> = self.agents.with_untracked(|agents| {
            agents
                .iter()
                .filter(|agent| agent.host_id == host_id)
                .map(|agent| agent.agent_id.clone())
                .collect()
        });
        let drop_set: HashSet<AgentId> = agent_ids.iter().cloned().collect();
        let mut removed_tab_ids = HashSet::new();
        self.center_zone.update(|center_zone| {
            removed_tab_ids.extend(close_host_runtime_tabs_in_cz(center_zone, host_id));
        });
        self.project_view_memory.update(|memories| {
            for (project, memory) in memories.iter_mut() {
                if let Some(center_zone) = memory.center_zone.as_mut() {
                    if project.host_id == host_id {
                        removed_tab_ids.extend(center_zone.all_tab_ids());
                    } else {
                        removed_tab_ids.extend(close_host_runtime_tabs_in_cz(center_zone, host_id));
                    }
                }
                if memory
                    .active_terminal
                    .as_ref()
                    .is_some_and(|active| active.host_id == host_id)
                {
                    memory.active_terminal = None;
                }
                memory.diff_contents.retain(|key, _| key.host_id != host_id);
            }
        });
        self.forget_removed_tab_occurrence_state(&removed_tab_ids);
        if !drop_set.is_empty() {
            self.chat_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.chat_tool_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.tool_progress.update(|map| {
                map.retain(|(id, _), _| !drop_set.contains(id));
            });
            self.chat_message_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.streaming_text.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_activity_stats.update(|map| {
                map.retain(|key, _| key.host_id != host_id);
            });
            self.task_token_usage.update(|map| {
                map.retain(|key, _| key.host_id != host_id);
            });
            self.session_history.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            // A host reset invalidates the transcript, so the marker index
            // (which holds `ChatRowId`s into it) has to go with it. The live
            // operation and capability snapshots go too: the authoritative
            // versions arrive on the reconnect's `AgentBootstrap`, and a
            // stale local copy would show a banner for an operation that may
            // have finished while we were disconnected.
            self.context_compaction_rows.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.context_compactions.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.terminal_compaction_operations.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.compaction_capability.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            // Team aggregates are host-keyed, so they go with the host rather
            // than with the agent drop set.
            self.team_context_compactions.update(|map| {
                map.retain(|(host, _), _| host != host_id);
            });
            self.task_lists.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.orchestration.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.interrupt_pending.update(|set| {
                set.retain(|id| !drop_set.contains(id));
            });
            self.last_turn_cancelled.update(|set| {
                set.retain(|id| !drop_set.contains(id));
            });
            self.agent_message_queue.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_turn_active.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
            self.agent_session_settings.update(|map| {
                map.retain(|id, _| !drop_set.contains(id));
            });
        }

        let compaction_ids: HashSet<AgentId> = self.compaction_in_progress.with_untracked(|map| {
            map.iter()
                .filter(|(_, info)| info.host_id == host_id)
                .map(|(agent_id, _)| agent_id.clone())
                .collect()
        });
        let mut compaction_drop_set = drop_set.clone();
        compaction_drop_set.extend(compaction_ids);
        self.compaction_in_progress.update(|map| {
            map.retain(|_, info| info.host_id != host_id);
        });
        self.compaction_errors.update(|map| {
            map.retain(|id, _| !compaction_drop_set.contains(id));
        });
        self.compaction_pending_completion.update(|map| {
            map.retain(|(host, _), _| host != host_id);
        });
        self.compaction_pending_close.update(|set| {
            set.retain(|(host, _)| host != host_id);
        });

        let review_ids: HashSet<ReviewId> = self.reviews.with_untracked(|reviews| {
            reviews
                .iter()
                .filter(|(_, review)| host_project_ids.contains(&review.project_id))
                .map(|(review_id, _)| review_id.clone())
                .collect()
        });
        let summary_review_ids: HashSet<ReviewId> =
            self.review_summaries.with_untracked(|summaries| {
                summaries
                    .iter()
                    .filter(|(project_id, _)| host_project_ids.contains(project_id))
                    .flat_map(|(_, summaries)| summaries.iter().map(|summary| summary.id.clone()))
                    .collect()
            });
        let mut host_review_ids = review_ids;
        host_review_ids.extend(summary_review_ids);

        self.file_tree.update(|map| {
            map.retain(|project_id, _| !host_project_ids.contains(project_id));
        });
        self.git_status.update(|map| {
            map.retain(|project_id, _| !host_project_ids.contains(project_id));
        });
        self.code_intel_overview.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.code_intel.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.open_files.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.pending_file_opens.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.diff_contents.update(|map| {
            map.retain(|key, _| key.host_id != host_id);
        });
        self.code_intel_navigate_ctx.update(|ctx| {
            if ctx.as_ref().is_some_and(|ctx| ctx.key.host_id == host_id) {
                *ctx = None;
            }
        });
        self.code_intel_hover.update(|hover| {
            if hover
                .as_ref()
                .is_some_and(|hover| hover.key.host_id == host_id)
            {
                *hover = None;
            }
        });
        self.references_state.update(|references| {
            if references
                .source_key
                .as_ref()
                .is_some_and(|key| key.host_id == host_id)
            {
                *references = ProjectReferencesUiState::default();
            }
        });
        self.review_summaries.update(|map| {
            map.retain(|project_id, _| !host_project_ids.contains(project_id));
        });
        self.reviews.update(|map| {
            map.retain(|_, review| !host_project_ids.contains(&review.project_id));
        });
        self.review_action_pending.update(|map| {
            map.retain(|review_id, _| !host_review_ids.contains(review_id));
        });
        self.review_action_target_pending.update(|set| {
            set.retain(|(review_id, _)| !host_review_ids.contains(review_id));
        });
        self.review_create_pending.update(|map| {
            map.retain(|(host, _), _| host != host_id);
        });
        // NOTE: Agents-view preferences (manual order + filters) are
        // intentionally NOT pruned here. They are server-owned durable state
        // replayed on the next bootstrap; pruning them on host cleanup is
        // exactly the behavior that produced the Agents-tab flicker/reset. The
        // non-persisted optimistic overlay is likewise left untouched — it is
        // reconciled by the next server notify/bootstrap, never by host
        // teardown. See `dev-docs/26-agent-organization.md` §5.5.
        self.sessions_panel_filters.update(|map| {
            map.retain(|active, _| {
                active
                    .as_ref()
                    .is_none_or(|active| active.host_id != host_id)
            });
        });

        self.host_streams.update(|streams| {
            streams.remove(host_id);
        });
        // Drop per-host validator state on both directions. Otherwise a
        // reconnect keeps stale seq/protocol stream state, but the server
        // builds fresh validators per connection and replays bootstraps from
        // seq 0.
        crate::send::clear_host_seqs(host_id);
        crate::dispatch::reset_inbound_state_for_host(host_id);
        self.command_errors_by_host.update(|errors| {
            errors.remove(host_id);
        });
        self.host_lifecycle_statuses.update(|statuses| {
            statuses.remove(host_id);
        });
        self.host_settings_by_host.update(|settings| {
            settings.remove(host_id);
        });
        self.host_settings_schema_by_host.update(|schemas| {
            schemas.remove(host_id);
        });
        self.configured_secrets_by_host.update(|secrets| {
            secrets.remove(host_id);
        });
        self.backend_setup_by_host.update(|setup| {
            setup.remove(host_id);
        });
        self.mobile_access_state.update(|map| {
            map.remove(host_id);
        });
        self.mobile_pairing_offer.update(|map| {
            map.remove(host_id);
        });
        self.mobile_pairing_start_pending.update(|set| {
            set.remove(host_id);
        });
        self.session_schemas.update(|schemas| {
            schemas.remove(host_id);
        });
        self.backend_config_schemas.update(|schemas| {
            schemas.remove(host_id);
        });
        self.backend_config_snapshots.update(|snapshots| {
            snapshots.remove(host_id);
        });
        // Capacity is never carried across a connection. A rehydrated figure
        // from a previous session would render as `Known` while being
        // arbitrarily old — quota moves, and a stale-but-confident number is
        // worse than an honest absence. The server replays the current snapshot
        // on the next subscribe.
        self.backend_capacity.update(|snapshots| {
            snapshots.remove(host_id);
        });
        self.backend_native_settings.update(|snapshots| {
            snapshots.remove(host_id);
        });
        self.native_settings_save_state.update(|states| {
            states.remove(host_id);
        });
        self.schemas_loaded_for_host.update(|loaded| {
            loaded.remove(host_id);
        });
        self.launch_profile_catalog.update(|map| {
            map.remove(host_id);
        });
        self.custom_agents.update(|map| {
            map.remove(host_id);
        });
        self.mcp_servers.update(|map| {
            map.remove(host_id);
        });
        self.steering.update(|map| {
            map.remove(host_id);
        });
        self.skills.update(|map| {
            map.remove(host_id);
        });
        self.workflow_summaries.update(|map| {
            map.remove(host_id);
        });
        self.workflow_diagnostics.update(|map| {
            map.remove(host_id);
        });
        self.workflow_runs.update(|map| {
            map.remove(host_id);
        });
        self.workflow_locations.update(|map| {
            map.remove(host_id);
        });
        self.teams.update(|map| {
            map.remove(host_id);
        });
        self.team_members.update(|map| {
            map.remove(host_id);
        });
        self.team_member_bindings.update(|map| {
            map.remove(host_id);
        });
        self.team_preset_catalogs.update(|map| {
            map.remove(host_id);
        });
        self.team_drafts.update(|map| {
            map.remove(host_id);
        });
        self.team_member_shuffle_suggestions.update(|map| {
            map.remove(host_id);
        });
        self.projects
            .update(|projects| projects.retain(|project| project.host_id != host_id));
        self.agents
            .update(|agents| agents.retain(|agent| agent.host_id != host_id));
        self.sessions
            .update(|sessions| sessions.retain(|session| session.host_id != host_id));
        self.terminals
            .update(|terminals| terminals.retain(|terminal| terminal.host_id != host_id));
        self.project_view_memory
            .update(|map| map.retain(|key, _| key.host_id != host_id));
        self.pending_workbench_creates
            .update(|pending| pending.retain(|entry| entry.host_id != host_id));
        self.pending_workbench_removes
            .update(|pending| pending.retain(|entry| entry.host_id != host_id));
        self.workbench_remove_prompt.update(|prompt| {
            if prompt
                .as_ref()
                .is_some_and(|prompt| prompt.host_id == host_id)
            {
                *prompt = None;
            }
        });
        self.pending_terminal_focus.update(|focus| {
            if focus.as_deref() == Some(host_id) {
                *focus = None;
            }
        });
        self.browse_dialog.update(|dialog| {
            if dialog
                .as_ref()
                .is_some_and(|dialog| dialog.host_id.get_untracked() == host_id)
            {
                *dialog = None;
            }
        });
        if self
            .active_terminal
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.host_id == host_id)
        {
            self.active_terminal.set(None);
        }
    }

    // ── Tab convenience methods ─────────────────────────────────────────

    pub fn open_tab(&self, content: TabContent, label: String, closeable: bool) -> Option<TabId> {
        let target = self
            .center_zone
            .with_untracked(|center_zone| center_zone.resolve(OpenTarget::Focused));
        self.open_tab_in(target, content, label, closeable)
    }

    #[cfg(all(test, target_arch = "wasm32"))]
    pub fn open_tab_at(
        &self,
        target: OpenTarget,
        content: TabContent,
        label: String,
        closeable: bool,
    ) -> Option<TabId> {
        let pane = self
            .center_zone
            .with_untracked(|center_zone| center_zone.resolve(target));
        self.open_tab_in(pane, content, label, closeable)
    }

    pub fn open_tab_in(
        &self,
        pane: PaneId,
        content: TabContent,
        label: String,
        closeable: bool,
    ) -> Option<TabId> {
        let tabs_enabled = self.tabs_enabled.get_untracked();
        let ratio = self.center_split_ratio.get_untracked();
        let mut result = None;
        let mut replaced_id = None;
        self.center_zone.update(|center_zone| {
            if let Some(existing) = center_zone.find_tab_in(pane, &content) {
                center_zone.activate(existing);
                result = Some(existing);
                return;
            }
            if let Some(existing) = center_zone.find_tab(&content) {
                center_zone.activate(existing);
                result = Some(existing);
                return;
            }
            if tabs_enabled {
                result = Some(center_zone.open_in(pane, content, label, closeable, ratio));
            } else {
                let id = center_zone.replace_active(content, label, closeable);
                replaced_id = Some(id);
                result = Some(id);
            }
        });
        if let Some(id) = replaced_id {
            self.forget_tab_scroll_state(id);
            // `replace_active` recycles the tab id for unrelated content, so
            // the previous occupant's draft must not survive into it. The
            // New Chat -> live agent upgrade goes through `update_tab`
            // instead and deliberately keeps its composer.
            self.forget_composers(&HashSet::from([id]));
        }
        result
    }

    pub fn set_split_ratio(&self, ratio: SplitRatio) {
        self.center_split_ratio.set(ratio);
        self.center_zone
            .update(|center_zone| center_zone.set_split_ratio(ratio));
    }

    pub fn focus_pane(&self, pane: PaneId) -> bool {
        let active = self
            .center_zone
            .with_untracked(|center_zone| center_zone.pane_active_tab_id(pane));
        let Some(active) = active else {
            return false;
        };
        self.activate_tab(active);
        true
    }

    pub fn duplicate_file_in_result(&self, target: PaneId, source: TabId) -> DuplicateFileResult {
        let eligibility = {
            let tabs_enabled = self.tabs_enabled.get_untracked();
            self.center_zone.with_untracked(|center_zone| {
                self.open_files.with_untracked(|open_files| {
                    duplicate_file_eligibility_for(
                        tabs_enabled,
                        center_zone,
                        open_files,
                        target,
                        source,
                    )
                })
            })
        };
        match eligibility {
            DuplicateFileEligibility::Enabled => {}
            DuplicateFileEligibility::TargetAlreadyContainsResource { existing } => {
                self.reveal_tab(existing);
                return DuplicateFileResult::ActivatedExisting {
                    source,
                    existing,
                    target,
                };
            }
            DuplicateFileEligibility::TabsDisabled => {
                return DuplicateFileResult::TabsDisabled;
            }
            DuplicateFileEligibility::SourceTabMissing => {
                return DuplicateFileResult::SourceTabMissing;
            }
            DuplicateFileEligibility::NotAFile => return DuplicateFileResult::NotAFile,
            DuplicateFileEligibility::NotLoaded => return DuplicateFileResult::NotLoaded,
        }

        let ratio = self.center_split_ratio.get_untracked();
        let mut result = None;
        self.center_zone.update(|center_zone| {
            result = center_zone.duplicate_file_to(source, target, ratio);
        });
        match result {
            Some(tab) => DuplicateFileResult::Duplicated {
                source,
                tab,
                target,
            },
            None => DuplicateFileResult::SourceTabMissing,
        }
    }

    pub fn move_tab_eligibility(&self, target: PaneId, id: TabId) -> MoveTabEligibility {
        self.center_zone
            .with_untracked(|center_zone| center_zone.move_tab_eligibility(target, id))
    }

    pub fn move_tab_to(&self, target: PaneId, id: TabId) -> MoveTabResult {
        match self.move_tab_eligibility(target, id) {
            MoveTabEligibility::Eligible => {}
            MoveTabEligibility::SourceTabMissing => return MoveTabResult::SourceTabMissing,
            MoveTabEligibility::AlreadyInTargetPane => {
                return MoveTabResult::AlreadyInTargetPane;
            }
            MoveTabEligibility::ResourceAlreadyInTarget { existing } => {
                return MoveTabResult::ResourceAlreadyInTarget { existing };
            }
        }
        let ratio = self.center_split_ratio.get_untracked();
        let mut result = MoveTabResult::SourceTabMissing;
        self.center_zone.update(|center_zone| {
            result = center_zone.move_tab_to(target, id, ratio);
        });
        result
    }

    pub fn split_tab_to(&self, target: PaneId, id: TabId) -> MoveTabResult {
        let ratio = self.center_split_ratio.get_untracked();
        let mut result = MoveTabResult::SourceTabMissing;
        self.center_zone.update(|center_zone| {
            result = center_zone.split_tab_to(target, id, ratio);
        });
        result
    }

    pub fn file_occurrence_in(&self, pane: PaneId, key: &FileResourceKey) -> Option<TabId> {
        self.center_zone.with_untracked(|center_zone| {
            center_zone.find_tab_in(pane, &TabContent::File { key: key.clone() })
        })
    }

    pub fn resolve_file_occurrence(
        &self,
        key: &FileResourceKey,
        preferred: PaneId,
    ) -> Option<(PaneId, TabId)> {
        self.center_zone.with_untracked(|center_zone| {
            center_zone
                .find_tab_in(preferred, &TabContent::File { key: key.clone() })
                .map(|tab| (preferred, tab))
                .or_else(|| {
                    center_zone
                        .find_tab_in(preferred.other(), &TabContent::File { key: key.clone() })
                        .map(|tab| (preferred.other(), tab))
                })
        })
    }

    /// Whether `tab` is still showing `key`'s contents at exactly `version`.
    ///
    /// Two tab kinds can host a code-intel occurrence. A **file** tab renders
    /// the file itself. A **diff** tab renders the new side of a diff whose
    /// text is byte-identical to that file on disk (see
    /// `diff_view::new_side_matches_worktree`), and holds the file's contents +
    /// subscription for the duration. Either way the contract is the same: the
    /// request/result only applies while the exact text it was computed against
    /// is what the user is looking at.
    pub fn file_occurrence_is_current(
        &self,
        tab: TabId,
        key: &FileResourceKey,
        version: ProjectFileVersion,
    ) -> bool {
        let occurrence_matches = self.center_zone.with_untracked(|center_zone| {
            center_zone
                .tab(tab)
                .is_some_and(|candidate| match &candidate.content {
                    TabContent::File { key: candidate_key } => candidate_key == key,
                    TabContent::Diff { .. } => self.diff_tab_holds_code_intel(tab, key),
                    _ => false,
                })
        });
        occurrence_matches
            && self
                .open_files
                .with_untracked(|files| files.get(key).is_some_and(|file| file.version == version))
    }

    /// Whether diff tab `tab` has pulled contents + a code-intel subscription
    /// for `key`.
    pub fn diff_tab_holds_code_intel(&self, tab: TabId, key: &FileResourceKey) -> bool {
        let code_intel_key = CodeIntelKey::from(key);
        self.diff_code_intel_holds.with_untracked(|holds| {
            holds
                .get(&code_intel_key)
                .is_some_and(|tabs| tabs.contains(&tab))
        })
    }

    /// Record that diff tab `tab` depends on `key`'s contents and code-intel
    /// subscription. Returns `true` when this is the first hold for that file
    /// from this tab, i.e. the caller still needs to issue the read + subscribe.
    pub fn hold_diff_code_intel(&self, tab: TabId, key: &FileResourceKey) -> bool {
        let code_intel_key = CodeIntelKey::from(key);
        let mut newly_held = false;
        self.diff_code_intel_holds.update(|holds| {
            newly_held = holds.entry(code_intel_key).or_default().insert(tab);
        });
        newly_held
    }

    /// Whether any diff tab still depends on `key`.
    fn diff_code_intel_is_held(&self, key: &FileResourceKey) -> bool {
        let code_intel_key = CodeIntelKey::from(key);
        self.diff_code_intel_holds.with_untracked(|holds| {
            holds
                .get(&code_intel_key)
                .is_some_and(|tabs| !tabs.is_empty())
        })
    }

    /// Drop `doomed`'s diff code-intel holds, returning the files that no diff
    /// tab holds any more. Bookkeeping only — the caller decides which of those
    /// files are now fully unreferenced and must be unsubscribed.
    fn release_diff_code_intel_holds(&self, doomed: &HashSet<TabId>) -> Vec<FileResourceKey> {
        let mut released = Vec::new();
        self.diff_code_intel_holds.update(|holds| {
            holds.retain(|key, tabs| {
                tabs.retain(|tab| !doomed.contains(tab));
                if tabs.is_empty() {
                    released.push(FileResourceKey {
                        host_id: key.host_id.clone(),
                        project_id: key.project_id.clone(),
                        path: key.path.clone(),
                    });
                    return false;
                }
                true
            });
        });
        released
    }

    pub fn target_file_navigation(&self, tab: TabId, navigation: PendingFileNavigation) {
        match navigation {
            PendingFileNavigation::Line(line) => self.pending_goto_line.set(Some((tab, line))),
            PendingFileNavigation::Offset(offset) => {
                self.pending_goto_offset.set(Some((tab, offset)))
            }
        }
    }

    pub fn record_pending_file_open(&self, key: FileResourceKey, intent: PendingFileOpen) {
        self.pending_file_opens.update(|pending| match intent {
            PendingFileOpen::Open { .. } => {
                pending.insert(key, intent);
            }
            PendingFileOpen::RefreshInPlace => {
                if !matches!(pending.get(&key), Some(PendingFileOpen::Open { .. })) {
                    pending.insert(key, intent);
                }
            }
        });
    }

    pub fn take_pending_file_open(&self, key: &FileResourceKey) -> Option<PendingFileOpen> {
        let mut intent = None;
        self.pending_file_opens.update(|pending| {
            intent = pending.remove(key);
        });
        intent
    }

    /// Insert `id` at the MRU front of `tab_lru`, dedup, truncate to
    /// `TAB_LRU_CAPACITY`. Visible pane actives are pinned separately by
    /// `mounted_tab_ids`, so switching in one pane cannot evict the other.
    pub fn bump_tab_lru(&self, id: TabId) {
        self.tab_lru.update(|lru| {
            lru.retain(|existing| *existing != id);
            lru.insert(0, id);
            if lru.len() > TAB_LRU_CAPACITY {
                lru.truncate(TAB_LRU_CAPACITY);
            }
        });
    }

    pub fn mounted_tab_ids(&self) -> Vec<TabId> {
        let lru = self.tab_lru.get();
        self.center_zone.with(|center_zone| {
            let pinned: HashSet<TabId> = center_zone
                .panes()
                .filter_map(|(_, pane)| pane.active_tab_id)
                .collect();
            center_zone
                .all_tabs()
                .filter(|(_, tab)| pinned.contains(&tab.id) || lru.contains(&tab.id))
                .map(|(_, tab)| tab.id)
                .collect()
        })
    }

    pub fn tab_scroll_state_untracked(&self, id: TabId) -> Option<TabScrollState> {
        self.tab_scroll_state
            .with_untracked(|scroll| scroll.get(&id).copied())
    }

    pub fn save_tab_scroll_state(&self, id: TabId, scroll_state: TabScrollState) {
        self.tab_scroll_state.update(|scroll| {
            scroll.insert(id, scroll_state);
        });
    }

    pub fn forget_tab_scroll_state(&self, id: TabId) {
        self.tab_scroll_state.update(|scroll| {
            scroll.remove(&id);
        });
    }

    pub(crate) fn forget_removed_tab_occurrence_state(&self, doomed: &HashSet<TabId>) {
        if doomed.is_empty() {
            return;
        }
        self.tab_lru.update(|lru| {
            lru.retain(|id| !doomed.contains(id));
        });
        self.tab_scroll_state.update(|scroll| {
            scroll.retain(|id, _| !doomed.contains(id));
        });
        self.forget_composers(doomed);
        self.code_intel_hover.update(|hover| {
            if hover
                .as_ref()
                .is_some_and(|hover| doomed.contains(&hover.tab))
            {
                *hover = None;
            }
        });
        self.code_intel_navigate_ctx.update(|context| {
            if context
                .as_ref()
                .is_some_and(|context| doomed.contains(&context.tab))
            {
                *context = None;
            }
        });
        // Bookkeeping only. `close_tabs` releases holds explicitly (before the
        // backing teardown) so it can unsubscribe what falls out; the paths that
        // reach here without it — project switch, restore — are dropping the
        // whole project stream anyway, so there is nothing to unsubscribe from.
        let _ = self.release_diff_code_intel_holds(doomed);
    }

    pub fn backing_release_projection(
        &self,
        doomed: &HashSet<TabId>,
    ) -> (HashSet<BackingResource>, HashSet<BackingResource>) {
        self.center_zone.with_untracked(|center_zone| {
            let survivors: HashSet<BackingResource> = center_zone
                .all_tabs()
                .filter(|(_, tab)| !doomed.contains(&tab.id))
                .filter_map(|(_, tab)| tab.backing_resource())
                .collect();
            let released = center_zone
                .all_tabs()
                .filter(|(_, tab)| doomed.contains(&tab.id))
                .filter_map(|(_, tab)| tab.backing_resource())
                .filter(|resource| !survivors.contains(resource))
                .collect();
            (survivors, released)
        })
    }

    fn tear_down_backing_resource(&self, resource: &BackingResource) {
        match resource {
            BackingResource::File(key) => {
                self.pending_file_opens.update(|pending| {
                    pending.remove(key);
                });
                // A diff tab may have pulled this file's contents and code-intel
                // subscription for its own rows. Closing the *file* tab must not
                // strip them out from under it — the diff view would silently
                // lose hover/go-to-definition with nothing to re-trigger a
                // resubscribe.
                if self.diff_code_intel_is_held(key) {
                    return;
                }
                self.open_files.update(|files| {
                    files.remove(key);
                });
                self.drop_code_intel(key);
            }
            BackingResource::Diff(key) => {
                self.diff_contents.update(|diffs| {
                    diffs.remove(key);
                });
            }
        }
    }

    /// Dismiss the hover popover and supersede any in-flight hover request so
    /// its late result is dropped (mirrors `actions::dismiss_hover`, which
    /// delegates here; the logic lives on state so tab-activation paths can
    /// dismiss without depending on the actions layer).
    pub fn dismiss_code_intel_hover(&self) {
        let mut id = 0;
        self.code_intel_request_seq.update(|seq| {
            *seq = seq.wrapping_add(1).max(1);
            id = *seq;
        });
        self.code_intel_active_hover.set(id);
        if self
            .code_intel_hover
            .with_untracked(|hover| hover.is_some())
        {
            self.code_intel_hover.set(None);
        }
    }

    fn drop_code_intel(&self, file: &FileResourceKey) {
        let key = CodeIntelKey {
            host_id: file.host_id.clone(),
            project_id: file.project_id.clone(),
            path: file.path.clone(),
        };
        self.code_intel.update(|map| {
            map.remove(&key);
        });
        #[cfg(target_arch = "wasm32")]
        {
            let host_id = file.host_id.clone();
            let stream = StreamPath(format!("/project/{}", file.project_id.0));
            let payload = protocol::CodeIntelUnsubscribeFilePayload {
                path: file.path.clone(),
            };
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = crate::send::send_frame(
                    &host_id,
                    stream,
                    protocol::FrameKind::CodeIntelUnsubscribeFile,
                    &payload,
                )
                .await
                {
                    log::error!("failed to send CodeIntelUnsubscribeFile: {error}");
                }
            });
        }
    }

    pub fn close_tabs(&self, doomed: HashSet<TabId>) {
        if doomed.is_empty() {
            return;
        }
        // Release the doomed diff tabs' code-intel holds *first*, so the file
        // teardown below sees an accurate holder set. Closing a file tab and the
        // diff tab holding the same file together must still tear the file down.
        let released_holds = self.release_diff_code_intel_holds(&doomed);
        let (survivors, released) = self.backing_release_projection(&doomed);
        for resource in &released {
            self.tear_down_backing_resource(resource);
        }
        // Files the diff viewer subscribed on its own. Now that no diff tab
        // holds them, they are unreferenced unless a file tab still backs one —
        // in which case that tab's own teardown owns it. Otherwise unsubscribe,
        // or the language server keeps the document open forever.
        for key in released_holds {
            let backing = BackingResource::File(key.clone());
            // Still open in a file tab, or already torn down by the loop above.
            if survivors.contains(&backing) || released.contains(&backing) {
                continue;
            }
            self.open_files.update(|files| {
                files.remove(&key);
            });
            self.drop_code_intel(&key);
        }
        self.forget_removed_tab_occurrence_state(&doomed);
        self.center_zone
            .update(|center_zone| center_zone.remove_tabs(&doomed));
    }

    pub fn close_tab(&self, id: TabId) {
        let closeable = self
            .center_zone
            .with_untracked(|center_zone| center_zone.tab(id).is_some_and(|tab| tab.closeable));
        if closeable {
            self.close_tabs(HashSet::from([id]));
        }
    }

    pub fn reveal_tab(&self, id: TabId) -> bool {
        let mut revealed = false;
        self.center_zone.update(|center_zone| {
            revealed = center_zone.reveal_tab(id);
        });
        // A hover popover is pinned to viewport coordinates captured over the
        // previously visible content; it has no meaning over another tab. Any
        // tab activation (mouse, keyboard, or programmatic) dismisses it and
        // supersedes an in-flight hover request.
        if revealed {
            self.dismiss_code_intel_hover();
        }
        revealed
    }

    pub fn update_tab(&self, id: TabId, content: TabContent, label: String) -> bool {
        let mut updated = false;
        self.center_zone.update(|center_zone| {
            updated = center_zone.update_tab(id, content, label);
        });
        updated
    }

    pub fn activate_tab(&self, id: TabId) {
        self.reveal_tab(id);
    }

    pub fn close_other_tabs(&self, id: TabId) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            let Some(pane_id) = center_zone.locate_tab(id) else {
                return HashSet::new();
            };
            center_zone
                .pane(pane_id)
                .into_iter()
                .flat_map(|pane| pane.tabs.iter())
                .filter(|tab| tab.id != id && tab.closeable)
                .map(|tab| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
        self.activate_tab(id);
    }

    pub fn close_tabs_to_right(&self, id: TabId) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            let Some(pane_id) = center_zone.locate_tab(id) else {
                return HashSet::new();
            };
            let Some(pane) = center_zone.pane(pane_id) else {
                return HashSet::new();
            };
            let Some(index) = pane.tabs.iter().position(|tab| tab.id == id) else {
                return HashSet::new();
            };
            pane.tabs[index + 1..]
                .iter()
                .filter(|tab| tab.closeable)
                .map(|tab| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
    }

    pub fn close_all_tabs(&self) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            center_zone
                .all_tabs()
                .filter(|(_, tab)| tab.closeable)
                .map(|(_, tab)| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
    }

    pub fn close_pane(&self, pane: PaneId) {
        let doomed = self.center_zone.with_untracked(|center_zone| {
            if !center_zone.is_split() {
                return HashSet::new();
            }
            center_zone
                .pane(pane)
                .into_iter()
                .flat_map(|pane| pane.tabs.iter())
                .map(|tab| tab.id)
                .collect()
        });
        self.close_tabs(doomed);
    }

    pub fn close_other_pane(&self) {
        let other = self
            .center_zone
            .with_untracked(|center_zone| center_zone.focused_id().other());
        self.close_pane(other);
    }

    pub fn rename_tab_label(&self, id: TabId, new_label: String) {
        self.center_zone
            .update(|center_zone| center_zone.rename_tab_label(id, new_label));
    }

    /// The team member a specific chat tab would spawn, if it is a draft
    /// awaiting one. Each pane's composer targets its own tab.
    pub fn tab_pending_team_member(&self, tab_id: TabId) -> Option<PendingTeamMember> {
        self.center_zone.with(|center_zone| {
            center_zone.tab(tab_id).and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: None,
                    pending_team_member: Some(pending),
                } => Some(pending.clone()),
                _ => None,
            })
        })
    }

    pub fn composer_pending_team_member_untracked(&self) -> Option<PendingTeamMember> {
        self.center_zone.with_untracked(|center_zone| {
            let (_, tab_id) = center_zone.composer_owner()?;
            center_zone.tab(tab_id).and_then(|tab| match &tab.content {
                TabContent::Chat {
                    agent_ref: None,
                    pending_team_member: Some(pending),
                } => Some(pending.clone()),
                _ => None,
            })
        })
    }
}
