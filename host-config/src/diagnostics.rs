//! Content-free, local desktop diagnostics; never sent to a host or provider.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiOperation {
    HostReceive,
    HostDispatch,
    Bridge,
    ComposerInput,
    ComposerResize,
    GlobalKeydown,
    Markdown,
    SyntaxHighlight,
    WorkerSubmit,
    WorkerReceive,
    FileTreeBuild,
    ChatRowProjection,
    ChatContextScan,
    ChatContextReportedScan,
    ChatInkLayout,
    ChatVirtualWindow,
    ChatTabBinding,
    AgentsSidebarProjection,
    HistoryProjectName,
    TaskPoll,
    TaskDrop,
    ProjectSwitch,
    TabOpen,
    TabClose,
    TabReveal,
}

pub const MAX_UI_SYNC_OPERATIONS: usize = 32;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSyncWork {
    pub calls: u32,
    pub items: u32,
    pub max_items: u32,
    pub work_ms: u32,
    pub longest_ms: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSyncOperationWork {
    pub operation: UiOperation,
    pub work: UiSyncWork,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTaskWork {
    pub polls: u32,
    pub poll_ms: u32,
    pub longest_poll_ms: u32,
    pub drops: u32,
    pub drop_ms: u32,
    pub longest_drop_ms: u32,
}

pub const MAX_UI_TASK_PRODUCERS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTaskProducer {
    Unattributed,
    ChatScrollRestore,
    ChatAutoScroll,
    ChatRowMeasure,
    TabStripVisibility,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTaskProducerWork {
    pub producer: UiTaskProducer,
    pub work: UiTaskWork,
}

pub const MAX_UI_ACTIVITY: usize = 4;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiTransition {
    ProjectSwitch,
    TabOpen,
    TabClose,
    TabReveal,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiFrameActivity {
    pub frame: FrameName,
    pub age_ms: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransitionActivity {
    pub transition: UiTransition,
    pub age_ms: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiActivityContext {
    pub commands: Vec<UiFrameActivity>,
    pub dispatch: Option<UiFrameActivity>,
    pub transitions: Vec<UiTransitionActivity>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendStartupPhase {
    WasmEntered,
    MountBegin,
    MountReturned,
    HighlightWarmupBegin,
    HighlightWarmupEnd,
    HostListenersReady,
    ConfiguredHostsRefreshReturned,
    HostConnectionsScheduled,
    ReadyAcknowledged,
}

/// A protocol frame kind name. This crate only bounds its shape to a
/// snake_case identifier; the desktop shell, which knows the protocol, admits a
/// record only when the name is a real frame kind.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FrameName(String);

impl FrameName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for FrameName {
    type Error = &'static str;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let valid = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if valid {
            Ok(Self(name))
        } else {
            Err("frame name must be a snake_case identifier")
        }
    }
}

impl From<FrameName> for String {
    fn from(name: FrameName) -> Self {
        name.0
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowUiWork {
    pub operation: UiOperation,
    pub duration_ms: u32,
    /// The host frame being handled, when the operation handles one.
    pub frame: Option<FrameName>,
    pub bytes: u32,
    pub items: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendDiagnostic {
    Startup {
        phase: FrontendStartupPhase,
        since_wasm_ms: u32,
    },
    EventLoopGap {
        interval_ms: u32,
        expected_ms: u32,
        focused: bool,
        longest_work: Option<SlowUiWork>,
        pending_bridge_calls: u32,
        task_work: UiTaskWork,
        task_producers: Vec<UiTaskProducerWork>,
        sync_work: Vec<UiSyncOperationWork>,
        activity: UiActivityContext,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendDiagnosticRecord {
    pub diagnostic: FrontendDiagnostic,
    pub dropped: u32,
}
