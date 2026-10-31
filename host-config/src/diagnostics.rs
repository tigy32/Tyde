//! Content-free, local desktop diagnostics; never sent to a host or provider.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Serialize, Deserialize)]
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
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendDiagnosticRecord {
    pub diagnostic: FrontendDiagnostic,
    pub dropped: u32,
}
