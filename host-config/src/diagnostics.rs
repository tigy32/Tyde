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

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowUiWork {
    pub operation: UiOperation,
    pub duration_ms: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendDiagnosticRecord {
    pub diagnostic: FrontendDiagnostic,
    pub dropped: u32,
}
