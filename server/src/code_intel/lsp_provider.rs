//! The generic LSP-backed code-intelligence provider.
//!
//! This is the **language-agnostic** machinery (spec §M7): it satisfies
//! [`CodeIntelProvider`](super::provider::CodeIntelProvider) and is internally an
//! **actor** (per "Actors Over Locks" and spec §2.3): one tokio task owns the
//! [`LspClient`] subprocess, the subscribed-file set, and the status state
//! machine. The trait methods are thin non-blocking sends onto the actor's
//! command channel.
//!
//! Everything here is shared across languages and reused **unchanged**; the only
//! per-language input is the [`LanguageServerConfig`] handed to
//! [`LspProvider::new`]. rust-analyzer ([`super::rust_analyzer`]) and pyright
//! ([`super::pyright`]) are two configs over this one engine — adding a third is
//! a new config, not a new code path.
//!
//! Lifecycle on the first subscribe for a root:
//!
//! 1. bootstrap (`config.discover`) — Found → spawn; Absent → `Unavailable`.
//! 2. `initialize` (advertise `publishDiagnostics`; `config.initialization_options`)
//!    → `initialized`.
//! 3. `textDocument/didOpen` the file with its on-disk contents.
//! 4. forward `textDocument/publishDiagnostics` as `code_intel_diagnostics`
//!    full-file replace snapshots, converting the server's UTF-16 line/character
//!    ranges to Tyde byte offsets **here** (the only place that conversion
//!    happens — the frontend never sees UTF-16).
//!
//! Status is mapped: `Starting` on spawn, `Indexing` after `initialized` /
//! while `$/progress` is active, `Ready` once indexing settles. Bootstrap
//! failure → `Unavailable` (with install hint) or `Failed`. An unexpected child
//! exit while files are subscribed → `Failed` + a bounded, backed-off restart
//! that re-spawns, re-initializes, and re-`didOpen`s the still-open files.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant as StdInstant};

use protocol::{
    ByteRange, CodeIntelCompleteness, CodeIntelDiagnostic, CodeIntelDiagnosticsPayload,
    CodeIntelErrorCode, CodeIntelErrorContext, CodeIntelErrorPayload, CodeIntelFileModelPayload,
    CodeIntelFindReferencesPayload, CodeIntelHoverPayload, CodeIntelHoverResultPayload,
    CodeIntelLanguageId, CodeIntelLocation, CodeIntelModelRange, CodeIntelNavigatePayload,
    CodeIntelNavigateResultPayload, CodeIntelOccurrence, CodeIntelProviderId,
    CodeIntelProviderStatus, CodeIntelReferenceLine, CodeIntelReferencesCompletePayload,
    CodeIntelReferencesFileResult, CodeIntelReferencesResultsPayload, CodeIntelResourceMode,
    CodeIntelRole, CodeIntelSetVisibleRangePayload, CodeIntelSeverity, CodeIntelState,
    CodeIntelStatusPayload, CodeIntelStatusScope, FrameKind, ProjectFileVersion, ProjectPath,
    ProjectRootPath,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use url::Url;

use super::language_server::{LanguageServerConfig, ServerDiscovery};
use super::lsp_client::{LspClient, LspError, LspEvent, LspRequester, LspServerExit};
use super::lsp_position::LineIndex;
use super::provider::CodeIntelProvider;
use super::{absolute_path, emit};
use crate::process_env;
use crate::stream::Stream;

/// Cap on bounded provider restart attempts after an unexpected child exit
/// (spec §M7 crash/restart). Resets to zero once the provider reaches `Ready`,
/// so a long-lived provider that crashes much later gets a fresh budget, while a
/// tight crash loop is bounded to avoid hammering a broken server.
const MAX_RESTART_ATTEMPTS: u32 = 3;

/// Bounded number of in-flight `textDocument/definition` requests per file's
/// background resolution, so the eager whole-file map never floods
/// rust-analyzer (spec §M3: "bounded concurrency").
const MAX_INFLIGHT_DEFINITIONS: usize = 8;

/// Coalesce resolved occurrences into a `code_intel_file_model` frame once this
/// many have resolved, so streaming targets don't flood the wire one frame per
/// occurrence.
const RESOLUTION_BATCH: usize = 32;

/// …or once this long has elapsed since the first un-flushed resolution, so a
/// trailing handful still ships promptly rather than waiting for a full batch.
const RESOLUTION_FLUSH: Duration = Duration::from_millis(50);

/// Minimum interval for high-volume `$/progress` report status updates. Phase
/// transitions and terminal errors still emit immediately through the same
/// typed status path; progress payload changes are emitted at this cadence.
const PROGRESS_STATUS_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Poll interval for detached provider jobs whose only cancellation signal is
/// an atomic active-id. It bounds how long a superseded/cancelled LSP request
/// can keep running before we send `$/cancelRequest`.
const REQUEST_CANCEL_POLL: Duration = Duration::from_millis(25);

/// Per-file cap on streamed references (M5). Beyond this, the file's result is
/// marked `truncated` so the UI shows the cap honestly rather than silently
/// dropping matches. Mirrors `search_project_files`' per-file cap.
const MAX_REFERENCES_PER_FILE: usize = 1000;

/// M6 large-file thresholds: a file at or above **either** bound has its model
/// delivered as transient `ByteRange` + `Partial` chunks (visible window first)
/// converging on an eventual `FullFile` + `Complete` model, instead of the M3
/// single-`FullFile` path. These are bounds on *delivery pacing*, never on
/// scope — the whole file is always covered.
const LARGE_FILE_BYTES: usize = 128 * 1024;
const LARGE_FILE_OCCURRENCES: usize = 1500;

/// Occurrences per transient `ByteRange` delivery chunk for a large file. The
/// chunk covering the visible window is streamed first (spec §M6).
const MODEL_CHUNK_OCCURRENCES: usize = 256;

/// Tunable thresholds for the large-file progressive-delivery path (M6). A field
/// on [`ModelJob`] rather than bare consts so tests can force the large path on
/// a small synthetic file (and shrink the chunk size) without fabricating a
/// 128 KiB fixture. Production always uses [`ModelTuning::DEFAULT`].
#[derive(Clone, Copy)]
struct ModelTuning {
    large_bytes: usize,
    large_occurrences: usize,
    chunk_occurrences: usize,
}

impl ModelTuning {
    const DEFAULT: ModelTuning = ModelTuning {
        large_bytes: LARGE_FILE_BYTES,
        large_occurrences: LARGE_FILE_OCCURRENCES,
        chunk_occurrences: MODEL_CHUNK_OCCURRENCES,
    };
}

/// Whether a file's model is delivered in transient `ByteRange` chunks (large)
/// rather than one `FullFile` frame. Whole-file scope is unaffected — this only
/// changes delivery pacing (spec §M6: "ByteRange is a transient delivery window,
/// not a gate").
fn is_large_file(text_len: usize, occurrence_count: usize, tuning: &ModelTuning) -> bool {
    text_len >= tuning.large_bytes || occurrence_count >= tuning.large_occurrences
}

/// Cap on concurrent in-flight `textDocument/definition` requests for one file's
/// background resolution. Tightened for large files, and further under a
/// `Limited`/`Unavailable` host, so a huge file can't flood rust-analyzer;
/// `Full` small files keep the M3 window. **Never zero** — resolution must
/// always make progress, so even the most constrained case still converges on
/// the whole file (just more slowly).
fn inflight_limit(resource_mode: CodeIntelResourceMode, large: bool) -> usize {
    match (resource_mode, large) {
        (CodeIntelResourceMode::Full, false) => MAX_INFLIGHT_DEFINITIONS,
        (CodeIntelResourceMode::Full, true) => 4,
        (CodeIntelResourceMode::Limited, false) => 2,
        (CodeIntelResourceMode::Limited, true) => 1,
        (CodeIntelResourceMode::Unavailable, _) => 1,
    }
}

/// Cap on resolved occurrences coalesced into one streamed `Partial` frame.
/// Smaller for large files (flush visible decorations sooner) and smaller still
/// under a constrained host (keep frames small) — but the final `Complete` frame
/// always covers the whole file regardless.
fn batch_limit(resource_mode: CodeIntelResourceMode, large: bool) -> usize {
    match resource_mode {
        CodeIntelResourceMode::Full => {
            if large {
                16
            } else {
                RESOLUTION_BATCH
            }
        }
        CodeIntelResourceMode::Limited | CodeIntelResourceMode::Unavailable => {
            if large {
                4
            } else {
                8
            }
        }
    }
}

/// Partition occurrence indices into transient `ByteRange` delivery chunks
/// (contiguous groups, since occurrences are decoded in document order), then
/// order the chunks so the one covering the visible window streams first. Pure
/// prioritization — every chunk is still delivered, so coverage stays
/// whole-file. With no visible hint, document order.
fn chunk_plan(
    occurrences: &[CodeIntelOccurrence],
    visible: Option<ByteRange>,
    chunk_occurrences: usize,
) -> Vec<Vec<usize>> {
    let chunk_size = chunk_occurrences.max(1);
    let mut chunks: Vec<Vec<usize>> = (0..occurrences.len())
        .collect::<Vec<_>>()
        .chunks(chunk_size)
        .map(<[usize]>::to_vec)
        .collect();
    if let Some(visible) = visible {
        // Stable partition: chunks intersecting the visible window first, each
        // group keeping document order (`sort_by_key` is stable; `false < true`).
        chunks.sort_by_key(|chunk| !chunk_intersects(chunk, occurrences, visible));
    }
    chunks
}

/// Whether any occurrence in `chunk` intersects the visible byte window.
fn chunk_intersects(
    chunk: &[usize],
    occurrences: &[CodeIntelOccurrence],
    visible: ByteRange,
) -> bool {
    chunk.iter().any(|&i| {
        let range = occurrences[i].range;
        range.start < visible.end && range.end > visible.start
    })
}

/// The bounding `ByteRange` of a non-empty set of occurrence indices — the
/// transient delivery window a `ByteRange` model frame advertises for that
/// chunk/batch. `None` for an empty set (nothing to advertise).
fn bounding_range(occurrences: &[CodeIntelOccurrence], indices: &[usize]) -> Option<ByteRange> {
    let mut iter = indices.iter().map(|&i| occurrences[i].range);
    let first = iter.next()?;
    let (mut start, mut end) = (first.start, first.end);
    for range in iter {
        start = start.min(range.start);
        end = end.max(range.end);
    }
    Some(ByteRange { start, end })
}

/// Trait-facing handle. Sends are non-blocking; all work happens on the actor.
/// One generic engine per project root, parameterized by a
/// [`LanguageServerConfig`] — rust-analyzer and pyright are two configs over the
/// same struct.
pub(crate) struct LspProvider {
    tx: mpsc::UnboundedSender<RaCommand>,
    /// Cached for the trait's `provider_id()` (e.g. `"rust-analyzer"`,
    /// `"pyright"`) without round-tripping the actor.
    provider_id: CodeIntelProviderId,
}

impl LspProvider {
    #[cfg(test)]
    pub(crate) fn new(
        config: LanguageServerConfig,
        root: ProjectRootPath,
        resource_mode: CodeIntelResourceMode,
    ) -> Self {
        Self::with_status_updates(config, root, resource_mode, None)
    }

    pub(crate) fn with_status_updates(
        config: LanguageServerConfig,
        root: ProjectRootPath,
        resource_mode: CodeIntelResourceMode,
        provider_status_tx: Option<mpsc::UnboundedSender<CodeIntelProviderStatus>>,
    ) -> Self {
        let provider_id = config.provider_id.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        // The actor keeps a clone of its own sender so the crash/restart path can
        // schedule a delayed `Restart` onto itself (spec §M7).
        let actor = RaActor::new(config, root, resource_mode, tx.clone(), provider_status_tx);
        tokio::spawn(actor.run(rx));
        Self { tx, provider_id }
    }
}

impl CodeIntelProvider for LspProvider {
    fn provider_id(&self) -> CodeIntelProviderId {
        self.provider_id.clone()
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(RaCommand::Shutdown);
    }

    fn reconfigure(&mut self, config: LanguageServerConfig) {
        self.provider_id = config.provider_id.clone();
        let _ = self.tx.send(RaCommand::Reconfigure { config });
    }

    fn subscribe(&mut self, path: ProjectPath, version: ProjectFileVersion, output: Stream) {
        let _ = self.tx.send(RaCommand::Subscribe {
            path,
            version,
            output,
        });
    }

    fn warm(&mut self) {
        let _ = self.tx.send(RaCommand::Warm);
    }

    fn unsubscribe(&mut self, path: &ProjectPath) {
        let _ = self.tx.send(RaCommand::Unsubscribe { path: path.clone() });
    }

    fn file_version_changed(&mut self, path: &ProjectPath, version: ProjectFileVersion) {
        let _ = self.tx.send(RaCommand::FileVersionChanged {
            path: path.clone(),
            version,
        });
    }

    fn set_visible_range(&mut self, payload: CodeIntelSetVisibleRangePayload) {
        let _ = self.tx.send(RaCommand::SetVisibleRange { payload });
    }

    fn navigate(&mut self, payload: CodeIntelNavigatePayload, output: Stream) {
        let _ = self.tx.send(RaCommand::Navigate { payload, output });
    }

    fn hover(&mut self, payload: CodeIntelHoverPayload, output: Stream) {
        let _ = self.tx.send(RaCommand::Hover { payload, output });
    }

    fn find_references(&mut self, payload: CodeIntelFindReferencesPayload, output: Stream) {
        let _ = self.tx.send(RaCommand::FindReferences { payload, output });
    }

    fn cancel_references(&mut self, references_id: u64) {
        let _ = self.tx.send(RaCommand::CancelReferences { references_id });
    }
}

impl Drop for LspProvider {
    fn drop(&mut self) {
        let _ = self.tx.send(RaCommand::Shutdown);
    }
}

enum RaCommand {
    Shutdown,
    Reconfigure {
        config: LanguageServerConfig,
    },
    Subscribe {
        path: ProjectPath,
        version: ProjectFileVersion,
        output: Stream,
    },
    Warm,
    Unsubscribe {
        path: ProjectPath,
    },
    FileVersionChanged {
        path: ProjectPath,
        version: ProjectFileVersion,
    },
    SetVisibleRange {
        payload: CodeIntelSetVisibleRangePayload,
    },
    Navigate {
        payload: CodeIntelNavigatePayload,
        output: Stream,
    },
    Hover {
        payload: CodeIntelHoverPayload,
        output: Stream,
    },
    FindReferences {
        payload: CodeIntelFindReferencesPayload,
        output: Stream,
    },
    CancelReferences {
        references_id: u64,
    },
    ModelFailed {
        path: ProjectPath,
        version: ProjectFileVersion,
    },
    /// Self-scheduled by the crash/restart path (spec §M7): re-bootstrap,
    /// re-spawn, re-initialize, and re-`didOpen` the still-subscribed files.
    Restart,
}

/// Internal lifecycle phase. `Cold` is the pre-start state (never emitted);
/// the rest map onto wire `CodeIntelState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Cold,
    Starting,
    Indexing,
    Ready,
    Failed,
    Unavailable,
}

impl Phase {
    fn wire_state(self) -> CodeIntelState {
        match self {
            Phase::Cold | Phase::Starting => CodeIntelState::Starting,
            Phase::Indexing => CodeIntelState::Indexing,
            Phase::Ready => CodeIntelState::Ready,
            Phase::Failed => CodeIntelState::Failed,
            Phase::Unavailable => CodeIntelState::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryContextError {
    NotReady,
    StaleVersion { actual: ProjectFileVersion },
}

struct StartFailure {
    code: CodeIntelErrorCode,
    message: String,
    hint: Option<String>,
    exit_status: Option<String>,
    stderr: Option<String>,
}

/// One subscribed file's tracked state.
struct SubscribedFile {
    version: ProjectFileVersion,
    /// Atomically mirrors `version` so detached on-demand LSP tasks can verify
    /// after awaiting that the source file has not changed under them.
    version_cell: Arc<AtomicU64>,
    output: Stream,
    /// The exact text sent in `didOpen`. The UTF-16→byte converter needs it to
    /// resolve diagnostic positions against the bytes the client is rendering.
    text: String,
    /// The absolute on-disk path. `publishDiagnostics` URIs are decoded back to
    /// a path and matched against this — robust to URI normalization
    /// (percent-encoding, casing) that a raw string compare would miss.
    absolute: PathBuf,
    /// The version for which we have already kicked off a whole-file model push
    /// (M3). `None` until the first push; a version change (re-read) resets it
    /// so the push restarts against the new contents.
    model_version: Option<ProjectFileVersion>,
}

/// The server-declared semantic-token legend captured from the `initialize`
/// result. The delta-encoded `semanticTokens` data references token types /
/// modifiers by **index into this legend**, so decoding occurrences needs it.
#[derive(Clone, Default)]
struct SemanticLegend {
    token_types: Vec<String>,
    token_modifiers: Vec<String>,
}

/// Handle to one file's in-flight background definition-resolution task (M3).
/// Held by the actor so it can reprioritize (viewport hint) or cancel
/// (unsubscribe / version supersession) the detached driver.
struct ResolutionHandle {
    /// The file version this resolution is producing frames for. A
    /// `set_visible_range` (or any frame) for a different version is ignored.
    version: ProjectFileVersion,
    /// Pushes a new visible byte range to the driver to reprioritize its queue.
    visible_tx: mpsc::UnboundedSender<ByteRange>,
    /// Dropping this cancels the driver: its `cancel_rx` resolves and the task
    /// returns without emitting further frames. Replacing or removing the handle
    /// (re-subscribe at a new version, unsubscribe) supersedes the old run.
    _cancel_tx: oneshot::Sender<()>,
}

struct RaActor {
    /// The per-language config (ids, lsp languageId, discovery,
    /// initializationOptions). The single language-specific input to this
    /// otherwise generic engine.
    config: LanguageServerConfig,
    /// A clone of the actor's own command sender, so the crash/restart path can
    /// schedule a delayed `RaCommand::Restart` onto itself without blocking the
    /// actor loop (spec §M7).
    self_tx: mpsc::UnboundedSender<RaCommand>,
    /// Bounded restart budget consumed by unexpected-exit recovery; reset to 0
    /// on reaching `Ready`. See [`MAX_RESTART_ATTEMPTS`].
    restart_attempts: u32,
    root: ProjectRootPath,
    /// The host's resource mode (spec §M6). Stamped on every status frame and
    /// handed to each model-push driver so large files pace their delivery
    /// without ever narrowing scope. Constant for the actor's lifetime today
    /// (the only host variable; see [`super::host_resource_mode`]).
    resource_mode: CodeIntelResourceMode,
    provider_status_tx: Option<mpsc::UnboundedSender<CodeIntelProviderStatus>>,
    warmed: bool,
    phase: Phase,
    message: Option<String>,
    client: Option<LspClient>,
    notifications: Option<mpsc::UnboundedReceiver<LspEvent>>,
    files: HashMap<ProjectPath, SubscribedFile>,
    /// Files for which `didOpen` has been sent (so we don't re-open).
    opened: HashSet<ProjectPath>,
    /// Active `$/progress` work tokens; non-empty ⇒ indexing in flight.
    active_progress: HashSet<String>,
    /// The server's semantic-token legend, captured at `initialize`.
    legend: SemanticLegend,
    /// Per-file background definition-resolution handles (M3 whole-file push).
    resolutions: HashMap<ProjectPath, ResolutionHandle>,
    /// The currently-active find-references query id for this root (M5),
    /// mirroring `search_project_files`' active-id atomic. `store`-ing a new id
    /// supersedes any in-flight query (its detached driver sees the mismatch and
    /// drops late frames); a cancel `compare_exchange`s it back to `0` so the
    /// driver emits a `cancelled` completion. `0` means "no active query".
    references_active: Arc<AtomicU64>,
    /// Latest on-demand navigate id seen by this provider. A newer navigate
    /// request supersedes an older in-flight `textDocument/definition` so the
    /// older LSP request can be proactively cancelled.
    navigate_active: Arc<AtomicU64>,
    /// Latest on-demand hover id seen by this provider. A newer hover request
    /// supersedes an older in-flight `textDocument/hover`.
    hover_active: Arc<AtomicU64>,
    /// Last provider-level status sent to the project overview. Exact repeats
    /// are dropped before they reach the project stream.
    last_provider_status: Option<CodeIntelProviderStatus>,
    /// Last file-scoped status sent per subscribed file. Exact repeats are
    /// dropped; version changes naturally produce a distinct payload.
    last_file_statuses: HashMap<ProjectPath, CodeIntelStatusPayload>,
    /// Last non-forced progress report emission. `$/progress` reports can arrive
    /// much faster than the UI can render, so they are coalesced here.
    last_progress_status_at: Option<StdInstant>,
}

impl RaActor {
    fn new(
        config: LanguageServerConfig,
        root: ProjectRootPath,
        resource_mode: CodeIntelResourceMode,
        self_tx: mpsc::UnboundedSender<RaCommand>,
        provider_status_tx: Option<mpsc::UnboundedSender<CodeIntelProviderStatus>>,
    ) -> Self {
        Self {
            config,
            self_tx,
            restart_attempts: 0,
            root,
            resource_mode,
            provider_status_tx,
            warmed: false,
            phase: Phase::Cold,
            message: None,
            client: None,
            notifications: None,
            files: HashMap::new(),
            opened: HashSet::new(),
            active_progress: HashSet::new(),
            legend: SemanticLegend::default(),
            resolutions: HashMap::new(),
            references_active: Arc::new(AtomicU64::new(0)),
            navigate_active: Arc::new(AtomicU64::new(0)),
            hover_active: Arc::new(AtomicU64::new(0)),
            last_provider_status: None,
            last_file_statuses: HashMap::new(),
            last_progress_status_at: None,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<RaCommand>) {
        loop {
            tokio::select! {
                command = rx.recv() => {
                    match command {
                        Some(RaCommand::Shutdown) => break,
                        Some(command) => self.handle_command(command).await,
                        None => break, // provider handle dropped
                    }
                }
                event = recv_event(&mut self.notifications) => {
                    match event {
                        Some(event) => self.handle_event(event),
                        None => {
                            // The LSP stdout closed: rust-analyzer exited. Stop
                            // polling the dead channel and surface the failure.
                            if self.notifications.is_some() {
                                self.notifications = None;
                                self.on_client_closed(LspServerExit::default());
                            }
                        }
                    }
                }
            }
        }
        if let Some(client) = self.client.take() {
            client.shutdown().await;
        }
    }

    async fn handle_command(&mut self, command: RaCommand) {
        match command {
            RaCommand::Shutdown => {}
            RaCommand::Reconfigure { config } => self.reconfigure(config).await,
            RaCommand::Subscribe {
                path,
                version,
                output,
            } => {
                // Whether this is a re-subscribe at a *new* version of an
                // already-open file: the contents changed under us, so any
                // in-flight resolution is stale and must restart (M3 version
                // supersession).
                let resubscribe = self.files.contains_key(&path);
                let version_changed = match self.files.get_mut(&path) {
                    // Re-subscribe of an already-tracked file: retarget the
                    // version + output stream. Preserve the `didOpen` text we
                    // already loaded — clobbering it with empties would leave a
                    // stale entry that no future `publishDiagnostics` can match
                    // (the file is already in `opened`, so `open_file` no-ops).
                    Some(existing) => {
                        let changed = existing.version != version;
                        existing.version = version;
                        existing.version_cell.store(version.0, Ordering::SeqCst);
                        existing.output = output;
                        existing.model_version = None;
                        changed
                    }
                    None => {
                        self.files.insert(
                            path.clone(),
                            SubscribedFile {
                                version,
                                version_cell: Arc::new(AtomicU64::new(version.0)),
                                output,
                                text: String::new(),
                                absolute: absolute_path(&path),
                                model_version: None,
                            },
                        );
                        false
                    }
                };
                if resubscribe {
                    self.last_file_statuses.remove(&path);
                }
                match self.phase {
                    Phase::Cold => match self.start().await {
                        Ok(()) => {
                            let paths: Vec<ProjectPath> = self.files.keys().cloned().collect();
                            for path in paths {
                                self.open_file(path).await;
                            }
                            self.ensure_file_models();
                        }
                        Err(failure) => self.emit_start_failure(failure, true),
                    },
                    Phase::Unavailable | Phase::Failed => {
                        // Re-emit the terminal status so the new file's chip is
                        // honest (no provider will come up for it).
                        self.emit_status_for(&path);
                    }
                    Phase::Starting | Phase::Indexing | Phase::Ready => {
                        self.emit_status_for(&path);
                        if version_changed {
                            self.reopen_file(&path).await;
                        } else {
                            self.open_file(path.clone()).await;
                        }
                        self.ensure_file_models();
                    }
                }
            }
            RaCommand::Warm => {
                self.warmed = true;
                match self.phase {
                    Phase::Cold => match self.start().await {
                        Ok(()) => {}
                        Err(failure) => self.emit_start_failure(failure, true),
                    },
                    Phase::Starting | Phase::Indexing | Phase::Ready => {}
                    Phase::Unavailable | Phase::Failed => {}
                }
            }
            RaCommand::Unsubscribe { path } => {
                // M1: keep the process alive after the last unsubscribe
                // (idle-shutdown policy is deferred, spec §9). Just stop
                // tracking the file so stale diagnostics aren't forwarded.
                self.files.remove(&path);
                self.opened.remove(&path);
                self.last_file_statuses.remove(&path);
                // Cancel any in-flight whole-file resolution (dropping the
                // handle drops its `_cancel_tx`, which the driver awaits).
                self.resolutions.remove(&path);
            }
            RaCommand::FileVersionChanged { path, version } => {
                // §M4: a watched change advanced this file's centralized
                // version. Re-read it, sync to rust-analyzer via `didChange`,
                // and restart the whole-file model push at the new version,
                // superseding the in-flight resolution for the old one. This is
                // the same machinery a re-subscribe at a new version uses, but
                // driven by the watcher and re-using the file's stored output
                // stream (no new subscribe).
                let Some(file) = self.files.get_mut(&path) else {
                    // Not subscribed here (or unsubscribed in between): ignore.
                    return;
                };
                // Monotonic: only advance, never regress. An equal version
                // (e.g. the client already re-subscribed at it, or the same
                // change reached us twice) is a no-op — no duplicate didChange,
                // no duplicate push.
                if version <= file.version {
                    return;
                }
                file.version = version;
                file.version_cell.store(version.0, Ordering::SeqCst);
                match self.phase {
                    Phase::Starting | Phase::Indexing | Phase::Ready => {
                        // Re-stamp the file's status at the new version, re-read
                        // + `didChange`, then (if Ready) restart the model push.
                        // While Indexing the push is deferred until Ready, at
                        // which point `ensure_file_models` runs against the new
                        // contents (publishDiagnostics after didChange carry the
                        // new state and are stamped with the new version).
                        self.emit_status_for(&path);
                        self.reopen_file(&path).await;
                        self.ensure_file_models();
                    }
                    Phase::Cold | Phase::Unavailable | Phase::Failed => {
                        // No live analysis to sync; the new version is recorded
                        // and a later start/subscribe resolves against it.
                    }
                }
            }
            RaCommand::SetVisibleRange { payload } => {
                // Pure prioritization hint: forward the visible byte range to the
                // file's resolution driver so it resolves on-screen occurrences
                // first. Never gates the whole-file scope. A range for a version
                // that doesn't match the in-flight resolution is ignored.
                if let Some(handle) = self.resolutions.get(&payload.path)
                    && handle.version == payload.version
                {
                    let _ = handle.visible_tx.send(payload.range);
                }
            }
            RaCommand::Navigate { payload, output } => self.on_navigate(payload, output),
            RaCommand::Hover { payload, output } => self.on_hover(payload, output),
            RaCommand::FindReferences { payload, output } => {
                self.on_find_references(payload, output)
            }
            RaCommand::CancelReferences { references_id } => {
                // Cancel only if this id is still the active query — a newer
                // query that already replaced it keeps running. Resetting to `0`
                // tells the driver it was cancelled (vs superseded), so it emits a
                // `cancelled: true` completion.
                let _ = self.references_active.compare_exchange(
                    references_id,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
            RaCommand::ModelFailed { path, version } => {
                if let Some(file) = self.files.get_mut(&path)
                    && file.version == version
                    && file.model_version == Some(version)
                {
                    file.model_version = None;
                }
                if self
                    .resolutions
                    .get(&path)
                    .is_some_and(|handle| handle.version == version)
                {
                    self.resolutions.remove(&path);
                }
            }
            RaCommand::Restart => self.restart().await,
        }
    }

    async fn reconfigure(&mut self, config: LanguageServerConfig) {
        if let Some(client) = self.client.take() {
            self.notifications = None;
            client.shutdown().await;
        } else {
            self.notifications = None;
        }

        self.config = config;
        self.restart_attempts = 0;
        self.phase = Phase::Cold;
        self.message = None;
        self.legend = SemanticLegend::default();
        self.opened.clear();
        self.active_progress.clear();
        self.last_progress_status_at = None;
        self.resolutions.clear();
        self.references_active.store(0, Ordering::SeqCst);
        self.navigate_active.store(0, Ordering::SeqCst);
        self.hover_active.store(0, Ordering::SeqCst);
        for file in self.files.values_mut() {
            file.text.clear();
            file.model_version = None;
        }

        if self.files.is_empty() && !self.warmed {
            return;
        }

        match self.start().await {
            Ok(()) => {
                let paths: Vec<ProjectPath> = self.files.keys().cloned().collect();
                for path in paths {
                    self.open_file(path).await;
                }
                self.ensure_file_models();
            }
            Err(failure) => self.emit_start_failure(failure, true),
        }
    }

    /// Bounded crash recovery (spec §M7): after the backoff elapsed, bring the
    /// provider back up from cold — re-bootstrap, re-spawn, re-initialize — then
    /// re-`didOpen` every still-subscribed file and restart its model push. A
    /// crash that left no subscribed files or project warmup to serve is a
    /// no-op. A re-spawn that itself fails transiently schedules a further
    /// backed-off attempt until the budget is spent, at which point we surface a
    /// fatal `ProviderCrashed`.
    async fn restart(&mut self) {
        if self.files.is_empty() && !self.warmed {
            return;
        }
        // Cold restart: the new child has nothing open and no legend yet.
        self.phase = Phase::Cold;
        self.legend = SemanticLegend::default();
        self.opened.clear();
        self.active_progress.clear();
        self.last_progress_status_at = None;
        self.resolutions.clear();
        match self.start().await {
            Ok(()) => {
                let paths: Vec<ProjectPath> = self.files.keys().cloned().collect();
                for path in paths {
                    self.open_file(path).await;
                }
                self.ensure_file_models();
            }
            Err(failure) => {
                let fatal = if failure.code == CodeIntelErrorCode::ProviderUnavailable {
                    true
                } else {
                    !self.schedule_restart()
                };
                self.emit_start_failure(failure, fatal);
            }
        }
        // `Unavailable` (the binary is genuinely gone) is terminal — retrying a
        // missing binary would just spin, so we leave the honest status up.
    }

    /// Schedule a backed-off `Restart` onto ourselves if the budget allows and
    /// there are still files or a warmed project to serve. Returns whether one
    /// was scheduled (so the caller can decide between a recoverable and a
    /// fatal crash error).
    fn schedule_restart(&mut self) -> bool {
        if (self.files.is_empty() && !self.warmed) || self.restart_attempts >= MAX_RESTART_ATTEMPTS
        {
            return false;
        }
        self.restart_attempts += 1;
        let delay = restart_backoff(self.restart_attempts);
        let tx = self.self_tx.clone();
        // Detached so the actor loop keeps serving other commands during the
        // backoff (non-blocking, per "Actors Over Locks").
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(RaCommand::Restart);
        });
        true
    }

    /// Streamed find-references (M5). Marks `references_id` the active query
    /// (superseding any prior), then spawns a detached driver that issues
    /// `textDocument/references`, groups locations by file, and streams one
    /// `code_intel_references_results` per file plus a terminal
    /// `code_intel_references_complete`. Running off the actor task keeps a slow
    /// LSP round-trip from stalling everything else. When the provider isn't
    /// `Ready` (or the file isn't open) we answer with an honest empty,
    /// non-error completion rather than blocking or fabricating results.
    fn on_find_references(&self, payload: CodeIntelFindReferencesPayload, output: Stream) {
        // Register this as the active query (supersede any in-flight one) before
        // any early return, so a later cancel for this id still resolves.
        self.references_active
            .store(payload.references_id, Ordering::SeqCst);
        let (requester, uri, text, _version_cell) =
            match self.query_context(&payload.path, payload.version) {
                Ok(context) => context,
                Err(QueryContextError::NotReady) => {
                    let complete = CodeIntelReferencesCompletePayload {
                        references_id: payload.references_id,
                        total_files: 0,
                        total_references: 0,
                        truncated: false,
                        cancelled: false,
                        error: None,
                    };
                    emit(&output, FrameKind::CodeIntelReferencesComplete, &complete);
                    return;
                }
                Err(QueryContextError::StaleVersion { actual }) => {
                    self.emit_stale_query_error(
                        &output,
                        CodeIntelErrorContext::FindReferences {
                            references_id: payload.references_id,
                            path: payload.path.clone(),
                        },
                        payload.version,
                        actual,
                    );
                    let complete = CodeIntelReferencesCompletePayload {
                        references_id: payload.references_id,
                        total_files: 0,
                        total_references: 0,
                        truncated: false,
                        cancelled: false,
                        error: Some(format!(
                            "stale code-intel request: client version {}, server version {}",
                            payload.version, actual
                        )),
                    };
                    emit(&output, FrameKind::CodeIntelReferencesComplete, &complete);
                    return;
                }
            };
        let job = FindReferencesJob {
            requester,
            root: self.root.clone(),
            references_id: payload.references_id,
            path: payload.path,
            uri,
            text,
            offset: payload.offset,
            include_declaration: payload.include_declaration,
            output,
            active: Arc::clone(&self.references_active),
        };
        tokio::spawn(job.run());
    }

    /// On-demand go-to-definition. Issues `textDocument/definition` at the
    /// requested byte position and streams a `code_intel_navigate_result`. The
    /// LSP round-trip runs on a detached task (so a slow server never stalls the
    /// actor); an empty `targets` is an honest answer for "not ready" / "no
    /// definition here", never a fabricated target.
    fn on_navigate(&self, payload: CodeIntelNavigatePayload, output: Stream) {
        self.navigate_active
            .store(payload.navigate_id, Ordering::SeqCst);
        let empty = |output: &Stream, payload: &CodeIntelNavigatePayload| {
            let result = CodeIntelNavigateResultPayload {
                navigate_id: payload.navigate_id,
                path: payload.path.clone(),
                version: payload.version,
                targets: Vec::new(),
            };
            emit(output, FrameKind::CodeIntelNavigateResult, &result);
        };
        let (requester, uri, text, version_cell) =
            match self.query_context(&payload.path, payload.version) {
                Ok(context) => context,
                Err(QueryContextError::NotReady) => {
                    empty(&output, &payload);
                    return;
                }
                Err(QueryContextError::StaleVersion { actual }) => {
                    self.emit_stale_query_error(
                        &output,
                        CodeIntelErrorContext::Navigate {
                            navigate_id: payload.navigate_id,
                            path: payload.path.clone(),
                        },
                        payload.version,
                        actual,
                    );
                    empty(&output, &payload);
                    return;
                }
            };
        let root = self.root.clone();
        let active = Arc::clone(&self.navigate_active);
        tokio::spawn(async move {
            let index = LineIndex::new(&text);
            let (line, character) = index.byte_to_position(payload.offset);
            let params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            });
            let targets = match request_while_active_id(
                &requester,
                "textDocument/definition",
                params,
                &active,
                payload.navigate_id,
            )
            .await
            {
                ActiveIdRequestOutcome::Response(Ok(value)) => {
                    let targets = locations_to_byte_targets(&root, value).await;
                    if active.load(Ordering::SeqCst) != payload.navigate_id {
                        return;
                    }
                    targets
                }
                ActiveIdRequestOutcome::Response(Err(error)) => {
                    tracing::debug!(%error, "code-intel: textDocument/definition failed");
                    if error.is_timeout() {
                        emit_query_error(
                            &output,
                            CodeIntelErrorCode::Timeout,
                            format!("textDocument/definition timed out: {error}"),
                            CodeIntelErrorContext::Navigate {
                                navigate_id: payload.navigate_id,
                                path: payload.path.clone(),
                            },
                        );
                    }
                    Vec::new()
                }
                ActiveIdRequestOutcome::Superseded => return,
            };
            if let Some(actual) = version_mismatch(&version_cell, payload.version) {
                emit_stale_query_error_frame(
                    &output,
                    CodeIntelErrorContext::Navigate {
                        navigate_id: payload.navigate_id,
                        path: payload.path.clone(),
                    },
                    payload.version,
                    actual,
                );
                let result = CodeIntelNavigateResultPayload {
                    navigate_id: payload.navigate_id,
                    path: payload.path,
                    version: payload.version,
                    targets: Vec::new(),
                };
                emit(&output, FrameKind::CodeIntelNavigateResult, &result);
                return;
            }
            let result = CodeIntelNavigateResultPayload {
                navigate_id: payload.navigate_id,
                path: payload.path,
                version: payload.version,
                targets,
            };
            emit(&output, FrameKind::CodeIntelNavigateResult, &result);
        });
    }

    /// On-demand hover. Issues `textDocument/hover` and streams a
    /// `code_intel_hover_result` with the markdown contents (and an optional
    /// byte range in the hovered file). `None` contents is an honest answer.
    fn on_hover(&self, payload: CodeIntelHoverPayload, output: Stream) {
        self.hover_active.store(payload.hover_id, Ordering::SeqCst);
        let empty = |output: &Stream, payload: &CodeIntelHoverPayload| {
            let result = CodeIntelHoverResultPayload {
                hover_id: payload.hover_id,
                path: payload.path.clone(),
                version: payload.version,
                contents: None,
                range: None,
            };
            emit(output, FrameKind::CodeIntelHoverResult, &result);
        };
        let (requester, uri, text, version_cell) =
            match self.query_context(&payload.path, payload.version) {
                Ok(context) => context,
                Err(QueryContextError::NotReady) => {
                    empty(&output, &payload);
                    return;
                }
                Err(QueryContextError::StaleVersion { actual }) => {
                    self.emit_stale_query_error(
                        &output,
                        CodeIntelErrorContext::Hover {
                            hover_id: payload.hover_id,
                            path: payload.path.clone(),
                        },
                        payload.version,
                        actual,
                    );
                    empty(&output, &payload);
                    return;
                }
            };
        let active = Arc::clone(&self.hover_active);
        tokio::spawn(async move {
            let index = LineIndex::new(&text);
            let (line, character) = index.byte_to_position(payload.offset);
            let params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            });
            let (contents, range) = match request_while_active_id(
                &requester,
                "textDocument/hover",
                params,
                &active,
                payload.hover_id,
            )
            .await
            {
                ActiveIdRequestOutcome::Response(Ok(value)) => {
                    let converted = convert_hover(&index, &value);
                    if active.load(Ordering::SeqCst) != payload.hover_id {
                        return;
                    }
                    converted
                }
                ActiveIdRequestOutcome::Response(Err(error)) => {
                    tracing::debug!(%error, "code-intel: textDocument/hover failed");
                    if error.is_timeout() {
                        emit_query_error(
                            &output,
                            CodeIntelErrorCode::Timeout,
                            format!("textDocument/hover timed out: {error}"),
                            CodeIntelErrorContext::Hover {
                                hover_id: payload.hover_id,
                                path: payload.path.clone(),
                            },
                        );
                    }
                    (None, None)
                }
                ActiveIdRequestOutcome::Superseded => return,
            };
            if let Some(actual) = version_mismatch(&version_cell, payload.version) {
                emit_stale_query_error_frame(
                    &output,
                    CodeIntelErrorContext::Hover {
                        hover_id: payload.hover_id,
                        path: payload.path.clone(),
                    },
                    payload.version,
                    actual,
                );
                let result = CodeIntelHoverResultPayload {
                    hover_id: payload.hover_id,
                    path: payload.path,
                    version: payload.version,
                    contents: None,
                    range: None,
                };
                emit(&output, FrameKind::CodeIntelHoverResult, &result);
                return;
            }
            let result = CodeIntelHoverResultPayload {
                hover_id: payload.hover_id,
                path: payload.path,
                version: payload.version,
                contents,
                range,
            };
            emit(&output, FrameKind::CodeIntelHoverResult, &result);
        });
    }

    /// Everything an on-demand definition/hover round-trip needs: a clonable
    /// request handle, the file's `file://` URI, and the exact text RA has open
    /// (for byte↔UTF-16 conversion). Returns `None` — so the caller answers
    /// honestly empty — unless the provider is `Ready`, the file is subscribed
    /// and `didOpen`'d, and the LSP client is live. This is the "never block,
    /// never fake" gate: during indexing or before the file is open we decline
    /// rather than guess.
    fn query_context(
        &self,
        path: &ProjectPath,
        expected_version: ProjectFileVersion,
    ) -> Result<(LspRequester, String, String, Arc<AtomicU64>), QueryContextError> {
        if self.phase != Phase::Ready {
            return Err(QueryContextError::NotReady);
        }
        if !self.opened.contains(path) {
            return Err(QueryContextError::NotReady);
        }
        let file = self.files.get(path).ok_or(QueryContextError::NotReady)?;
        if file.version != expected_version {
            return Err(QueryContextError::StaleVersion {
                actual: file.version,
            });
        }
        let text = file.text.clone();
        let version_cell = Arc::clone(&file.version_cell);
        let uri = file_uri(&file.absolute).ok_or(QueryContextError::NotReady)?;
        let requester = self
            .client
            .as_ref()
            .ok_or(QueryContextError::NotReady)?
            .requester();
        Ok((requester, uri, text, version_cell))
    }

    fn emit_stale_query_error(
        &self,
        output: &Stream,
        context: CodeIntelErrorContext,
        expected: ProjectFileVersion,
        actual: ProjectFileVersion,
    ) {
        emit_stale_query_error_frame(output, context, expected, actual);
    }

    /// Cold → running: discover, spawn, handshake. Returns whether the provider
    /// is usable (and files should be opened), leaving error emission to the
    /// caller so restart attempts can mark failures fatal only after the bounded
    /// retry budget is exhausted.
    async fn start(&mut self) -> Result<(), StartFailure> {
        self.set_phase(Phase::Starting, None);

        let discover = self.config.discover;
        let configured_path = self.config.configured_path.clone();
        let cwd = PathBuf::from(&self.root.0);
        let discover_cwd = cwd.clone();
        let discovery = match tokio::task::spawn_blocking(move || {
            discover(&discover_cwd, configured_path.as_ref())
        })
        .await
        {
            Ok(discovery) => discovery,
            Err(error) => {
                let message = format!("language-server discovery task failed: {error}");
                self.set_phase(Phase::Failed, Some(message.clone()));
                return Err(StartFailure {
                    code: CodeIntelErrorCode::Internal,
                    message,
                    hint: None,
                    exit_status: None,
                    stderr: None,
                });
            }
        };
        let (binary, args) = match discovery {
            ServerDiscovery::Found { binary, args } => (binary, args),
            ServerDiscovery::Absent {
                message,
                hint,
                exit_status,
                stderr,
            } => {
                self.set_phase(Phase::Unavailable, Some(message.clone()));
                return Err(StartFailure {
                    code: CodeIntelErrorCode::ProviderUnavailable,
                    message,
                    hint: Some(hint),
                    exit_status,
                    stderr,
                });
            }
        };

        let env_path = process_env::resolved_child_process_path();
        let (client, notifications) = match LspClient::spawn(&binary, &args, &cwd, env_path).await {
            Ok(pair) => pair,
            Err(error) => {
                self.set_phase(Phase::Failed, Some(error.clone()));
                return Err(StartFailure {
                    code: CodeIntelErrorCode::ProviderCrashed,
                    message: error,
                    hint: None,
                    exit_status: None,
                    stderr: None,
                });
            }
        };
        self.client = Some(client);
        self.notifications = Some(notifications);

        let init_params = initialize_params(&self.root, (self.config.initialization_options)());
        let init_result = {
            let client = self.client.as_ref().expect("client just set");
            client.request("initialize", init_params).await
        };
        let init_value = match init_result {
            Ok(value) => value,
            Err(error) => {
                let code = if error.is_timeout() {
                    CodeIntelErrorCode::Timeout
                } else {
                    CodeIntelErrorCode::ProviderCrashed
                };
                let message = format!("initialize failed: {error}");
                self.set_phase(Phase::Failed, Some(message.clone()));
                let failure = StartFailure {
                    code,
                    message,
                    hint: None,
                    exit_status: error.exit_status,
                    stderr: error.stderr,
                };
                self.client = None;
                self.notifications = None;
                return Err(failure);
            }
        };
        // Capture the server's semantic-token legend so M3 can decode the
        // delta-encoded `semanticTokens` data into occurrence ranges.
        self.legend = parse_semantic_legend(&init_value);
        if let Some(client) = self.client.as_ref() {
            let _ = client.notify("initialized", json!({}));
        }
        // Handshake done; rust-analyzer now begins indexing.
        self.set_phase(Phase::Indexing, None);
        Ok(())
    }

    async fn open_file(&mut self, path: ProjectPath) {
        if self.opened.contains(&path) {
            return;
        }
        let absolute = absolute_path(&path);
        let text = match tokio::fs::read_to_string(&absolute).await {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, ?absolute, "code-intel: failed to read file for didOpen");
                return;
            }
        };
        let Some(uri) = file_uri(&absolute) else {
            tracing::warn!(
                ?absolute,
                "code-intel: could not build a file URI for didOpen"
            );
            return;
        };
        let version = match self.files.get_mut(&path) {
            Some(file) => {
                file.text = text.clone();
                file.version.0 as i64
            }
            None => return, // unsubscribed between command and read
        };

        let params = json!({
            "textDocument": {
                "uri": uri,
                "languageId": self.config.lsp_language_id,
                "version": version,
                "text": text,
            }
        });
        if let Some(client) = self.client.as_ref() {
            let _ = client.notify("textDocument/didOpen", params);
        }
        self.opened.insert(path);
    }

    /// Re-read a file that changed under us (a re-subscribe at a new version)
    /// and notify rust-analyzer with a full-document `didChange`, then clear its
    /// `model_version` so the whole-file model push restarts against the new
    /// contents (M3 supersession). Falls back to a first `didOpen` if the file
    /// somehow isn't open yet.
    async fn reopen_file(&mut self, path: &ProjectPath) {
        if !self.opened.contains(path) {
            self.open_file(path.clone()).await;
            return;
        }
        let absolute = absolute_path(path);
        let text = match tokio::fs::read_to_string(&absolute).await {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, ?absolute, "code-intel: failed to re-read file for didChange");
                return;
            }
        };
        let Some(uri) = file_uri(&absolute) else {
            return;
        };
        let version = match self.files.get_mut(path) {
            Some(file) => {
                file.text = text.clone();
                // Force the model push to restart for the new contents.
                file.model_version = None;
                file.version.0 as i64
            }
            None => return, // unsubscribed between command and read
        };
        let params = json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }],
        });
        if let Some(client) = self.client.as_ref() {
            let _ = client.notify("textDocument/didChange", params);
        }
    }

    /// Kick off the whole-file semantic-model push for every opened file that
    /// hasn't been pushed at its current version yet. Runs only while `Ready`
    /// (semantic tokens / definition need a settled analysis). Idempotent:
    /// `model_version` guards against re-pushing the same (file, version).
    fn ensure_file_models(&mut self) {
        if self.phase != Phase::Ready {
            return;
        }
        let pending: Vec<ProjectPath> = self
            .files
            .iter()
            .filter(|(path, file)| {
                self.opened.contains(*path) && file.model_version != Some(file.version)
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in pending {
            self.start_file_model(path);
        }
    }

    /// Spawn the detached driver that pushes one file's whole-file model: an
    /// immediate `Partial` occurrence set from `semanticTokens/full`, then
    /// streamed `definition` targets, then a final `Complete` frame. Replacing
    /// the handle cancels any prior in-flight resolution for this path.
    fn start_file_model(&mut self, path: ProjectPath) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let Some(file) = self.files.get_mut(&path) else {
            return;
        };
        let Some(uri) = file_uri(&file.absolute) else {
            tracing::warn!(?path, "code-intel: could not build file URI for model push");
            return;
        };
        file.model_version = Some(file.version);

        let (visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let job = ModelJob {
            requester: client.requester(),
            root: self.root.clone(),
            path: path.clone(),
            version: file.version,
            uri,
            text: file.text.clone(),
            provider: self.config.provider_id.clone(),
            language: self.config.language.clone(),
            output: file.output.clone(),
            legend: self.legend.clone(),
            resource_mode: self.resource_mode,
            tuning: ModelTuning::DEFAULT,
            visible_rx,
            cancel_rx,
            model_failed_tx: Some(self.self_tx.clone()),
        };
        // Inserting replaces (and drops) any prior handle, whose `_cancel_tx`
        // drop cancels the superseded driver.
        self.resolutions.insert(
            path,
            ResolutionHandle {
                version: job.version,
                visible_tx,
                _cancel_tx: cancel_tx,
            },
        );
        tokio::spawn(job.run());
    }

    fn handle_event(&mut self, event: LspEvent) {
        match event {
            LspEvent::Notification(note) => match note.method.as_str() {
                "textDocument/publishDiagnostics" => self.on_publish_diagnostics(note.params),
                "$/progress" => self.on_progress(note.params),
                _ => {}
            },
            LspEvent::ProtocolError(message) => self.on_protocol_error(message),
            LspEvent::ServerExited(exit) => {
                self.notifications = None;
                self.on_client_closed(exit);
            }
        }
    }

    fn on_publish_diagnostics(&mut self, params: Value) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return;
        };
        // Decode the incoming URI back to a path and match by path, so URI
        // normalization (percent-encoding of spaces / non-ASCII, casing) by
        // rust-analyzer can't cause a silent miss.
        let Some(incoming) = uri_to_path(uri) else {
            tracing::debug!(%uri, "code-intel: ignoring publishDiagnostics with unparseable URI");
            return;
        };
        let Some((path, version, output, text)) = self.files.iter().find_map(|(path, file)| {
            (file.absolute == incoming).then(|| {
                (
                    path.clone(),
                    file.version,
                    file.output.clone(),
                    file.text.clone(),
                )
            })
        }) else {
            // Diagnostics for an unsubscribed workspace file: ignore in M1.
            return;
        };

        let index = LineIndex::new(&text);
        let diagnostics = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| convert_diagnostic(&index, item))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let payload = CodeIntelDiagnosticsPayload {
            path,
            version,
            diagnostics,
        };
        emit(&output, FrameKind::CodeIntelDiagnostics, &payload);

        // Diagnostics arriving while no progress is in flight means the initial
        // analysis settled — promote to Ready. M1-acceptable heuristic: this and
        // the `$/progress` end transition are approximate signals for "ready",
        // not an authoritative one; may be refined later.
        if self.phase == Phase::Indexing && self.active_progress.is_empty() {
            self.set_phase(Phase::Ready, None);
        }
    }

    fn on_progress(&mut self, params: Value) {
        let token = match params.get("token") {
            Some(Value::String(token)) => token.clone(),
            Some(Value::Number(token)) => token.to_string(),
            _ => return,
        };
        let Some(value) = params.get("value") else {
            return;
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                self.active_progress.insert(token);
                let phase_changed = self.phase != Phase::Indexing;
                self.phase = Phase::Indexing;
                self.message = None;
                let (done, total) = progress_amounts(value);
                self.emit_status_all(done, total, phase_changed);
            }
            Some("report") => {
                let (done, total) = progress_amounts(value);
                self.emit_status_all(done, total, false);
            }
            Some("end") => {
                self.active_progress.remove(&token);
                if self.active_progress.is_empty() {
                    self.set_phase(Phase::Ready, None);
                }
            }
            _ => {}
        }
    }

    /// Malformed traffic from rust-analyzer (bad framing / oversize / invalid
    /// JSON). Surface a typed `code_intel_error` (ProtocolError) and move to
    /// Failed — never a silent log. Deduped: if already Failed, do nothing so a
    /// burst of corrupt frames doesn't spam error frames.
    fn on_protocol_error(&mut self, message: String) {
        if self.phase == Phase::Failed {
            return;
        }
        let payload = CodeIntelErrorPayload {
            code: CodeIntelErrorCode::ProtocolError,
            message: message.clone(),
            hint: None,
            exit_status: None,
            stderr: None,
            context: CodeIntelErrorContext::Provider {
                language: self.config.language.clone(),
            },
            fatal: true,
        };
        for file in self.files.values() {
            emit(&file.output, FrameKind::CodeIntelError, &payload);
        }
        self.set_phase(Phase::Failed, Some(message));
    }

    /// The language-server child exited unexpectedly while we were driving it.
    /// Surface the crash visibly (`Failed` status + a `ProviderCrashed` error —
    /// never a silent empty model) and, if files are still subscribed and the
    /// restart budget allows, schedule a bounded backed-off restart (spec §M7).
    /// The error is `fatal: false` while a restart is pending (the scope will
    /// recover) and `fatal: true` once the budget is exhausted.
    fn on_client_closed(&mut self, exit: LspServerExit) {
        self.client = None;
        // The dead child has nothing open; drop in-flight resolution so the
        // restart re-pushes against a fresh process rather than leaking tasks.
        self.opened.clear();
        self.resolutions.clear();
        self.active_progress.clear();
        let scheduled = self.schedule_restart();
        let message = language_server_exit_message();
        self.set_phase(Phase::Failed, Some(message.clone()));
        self.emit_provider_crashed(!scheduled, message, exit.exit_status, exit.stderr);
    }

    /// Emit a typed `ProviderCrashed` error to every subscribed file's stream.
    /// Pairs with the `Failed` status from [`on_client_closed`] so a crash is
    /// always visible, never a silent empty model (spec §4.4).
    fn emit_provider_crashed(
        &self,
        fatal: bool,
        message: String,
        exit_status: Option<String>,
        stderr: Option<String>,
    ) {
        self.emit_provider_error(
            CodeIntelErrorCode::ProviderCrashed,
            message,
            fatal,
            None,
            exit_status,
            stderr,
        );
    }

    fn emit_start_failure(&self, failure: StartFailure, fatal: bool) {
        self.emit_provider_error(
            failure.code,
            failure.message,
            fatal,
            failure.hint,
            failure.exit_status,
            failure.stderr,
        );
    }

    fn emit_provider_error(
        &self,
        code: CodeIntelErrorCode,
        message: String,
        fatal: bool,
        hint: Option<String>,
        exit_status: Option<String>,
        stderr: Option<String>,
    ) {
        let payload = CodeIntelErrorPayload {
            code,
            message,
            hint,
            exit_status,
            stderr,
            context: CodeIntelErrorContext::Provider {
                language: self.config.language.clone(),
            },
            fatal,
        };
        for file in self.files.values() {
            emit(&file.output, FrameKind::CodeIntelError, &payload);
        }
    }

    fn set_phase(&mut self, phase: Phase, message: Option<String>) {
        self.phase = phase;
        self.message = message;
        self.emit_status_all(None, None, true);
        // Becoming Ready is the trigger to push whole-file models for any
        // opened files that haven't been pushed at their current version, and to
        // refresh the crash-restart budget (the provider proved healthy again).
        if self.phase == Phase::Ready {
            self.restart_attempts = 0;
            self.ensure_file_models();
        }
    }

    fn emit_status_all(&mut self, work_done: Option<u32>, total_work: Option<u32>, force: bool) {
        if !force && self.progress_status_is_throttled(work_done, total_work) {
            return;
        }
        self.emit_provider_status(work_done, total_work);
        let paths = self.files.keys().cloned().collect::<Vec<_>>();
        for path in paths {
            self.emit_one_status_for_path(&path, work_done, total_work);
        }
    }

    fn progress_status_is_throttled(
        &mut self,
        work_done: Option<u32>,
        total_work: Option<u32>,
    ) -> bool {
        if work_done.is_none() && total_work.is_none() {
            return false;
        }
        let now = StdInstant::now();
        if self
            .last_progress_status_at
            .is_some_and(|last| now.duration_since(last) < PROGRESS_STATUS_MIN_INTERVAL)
        {
            return true;
        }
        self.last_progress_status_at = Some(now);
        false
    }

    fn emit_provider_status(&mut self, work_done: Option<u32>, total_work: Option<u32>) {
        let Some(tx) = &self.provider_status_tx else {
            return;
        };
        let status = CodeIntelProviderStatus {
            provider: self.config.provider_id.clone(),
            language: self.config.language.clone(),
            state: self.phase.wire_state(),
            resource_mode: self.resource_mode,
            work_done,
            total_work,
            message: self.message.clone(),
        };
        if self.last_provider_status.as_ref() == Some(&status) {
            return;
        }
        self.last_provider_status = Some(status.clone());
        let _ = tx.send(status);
    }

    fn emit_status_for(&mut self, path: &ProjectPath) {
        self.emit_one_status_for_path(path, None, None);
    }

    fn emit_one_status_for_path(
        &mut self,
        path: &ProjectPath,
        work_done: Option<u32>,
        total_work: Option<u32>,
    ) {
        let Some(file) = self.files.get(path) else {
            return;
        };
        let version = file.version;
        let output = file.output.clone();
        let status = CodeIntelStatusPayload {
            scope: CodeIntelStatusScope::File {
                path: path.clone(),
                version,
            },
            state: self.phase.wire_state(),
            resource_mode: self.resource_mode,
            work_done,
            total_work,
            message: self.message.clone(),
        };
        if self.last_file_statuses.get(path) == Some(&status) {
            return;
        }
        self.last_file_statuses.insert(path.clone(), status.clone());
        emit(&output, FrameKind::CodeIntelStatus, &status);
    }
}

fn emit_query_error(
    output: &Stream,
    code: CodeIntelErrorCode,
    message: String,
    context: CodeIntelErrorContext,
) {
    let payload = CodeIntelErrorPayload {
        code,
        message,
        hint: None,
        exit_status: None,
        stderr: None,
        context,
        fatal: false,
    };
    emit(output, FrameKind::CodeIntelError, &payload);
}

fn emit_stale_query_error_frame(
    output: &Stream,
    context: CodeIntelErrorContext,
    expected: ProjectFileVersion,
    actual: ProjectFileVersion,
) {
    emit_query_error(
        output,
        CodeIntelErrorCode::StaleVersion,
        format!("stale code-intel request: client version {expected}, server version {actual}"),
        context,
    );
}

fn version_mismatch(
    version_cell: &AtomicU64,
    expected: ProjectFileVersion,
) -> Option<ProjectFileVersion> {
    let actual = ProjectFileVersion(version_cell.load(Ordering::SeqCst));
    (actual != expected).then_some(actual)
}

fn language_server_exit_message() -> String {
    "language server exited unexpectedly".to_owned()
}

/// Await the next LSP event, or pend forever when there is no client yet.
async fn recv_event(
    notifications: &mut Option<mpsc::UnboundedReceiver<LspEvent>>,
) -> Option<LspEvent> {
    match notifications.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending::<Option<LspEvent>>().await,
    }
}

/// Backoff before the Nth bounded restart attempt (spec §M7): exponential from
/// 500ms, capped at 8s, so a crash loop is spaced out rather than hammering a
/// broken server.
#[cfg(not(test))]
fn restart_backoff(attempt: u32) -> Duration {
    let factor = 1u64 << attempt.min(5);
    Duration::from_millis((500u64.saturating_mul(factor)).min(8000))
}

#[cfg(test)]
fn restart_backoff(attempt: u32) -> Duration {
    Duration::from_millis(u64::from(attempt.max(1)))
}

/// Build the `initialize` params: advertise `publishDiagnostics` + semantic
/// tokens + work-done progress (all language-agnostic LSP capabilities), and
/// pass the language's `initializationOptions` through verbatim (rust-analyzer's
/// cargo build-scripts / proc-macros, pyright's analysis config, …).
fn initialize_params(root: &ProjectRootPath, initialization_options: Value) -> Value {
    let root_uri = file_uri(Path::new(&root.0)).unwrap_or_else(|| format!("file://{}", root.0));
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "textDocument": {
                "publishDiagnostics": { "relatedInformation": true },
                "synchronization": { "dynamicRegistration": false, "didSave": false },
                // Advertise semantic tokens so the server enables the feature and
                // returns its legend in `initialize` (M3 occurrence set). The
                // server's legend — captured from the result — is what we decode
                // against; the lists here only state what we understand.
                "semanticTokens": {
                    "dynamicRegistration": false,
                    "requests": { "range": false, "full": { "delta": false } },
                    "tokenTypes": CLIENT_TOKEN_TYPES,
                    "tokenModifiers": CLIENT_TOKEN_MODIFIERS,
                    "formats": ["relative"],
                    "overlappingTokenSupport": false,
                    "multilineTokenSupport": false
                }
            },
            "window": { "workDoneProgress": true }
        },
        "initializationOptions": initialization_options
    })
}

/// The standard LSP `SemanticTokenTypes` we advertise understanding of. The
/// server picks its own legend regardless; this only satisfies the capability
/// shape so rust-analyzer turns the feature on.
const CLIENT_TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "class",
    "enum",
    "interface",
    "struct",
    "typeParameter",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "event",
    "function",
    "method",
    "macro",
    "keyword",
    "modifier",
    "comment",
    "string",
    "number",
    "regexp",
    "operator",
    "decorator",
];

/// The standard LSP `SemanticTokenModifiers` we advertise understanding of.
const CLIENT_TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "definition",
    "readonly",
    "static",
    "deprecated",
    "abstract",
    "async",
    "modification",
    "documentation",
    "defaultLibrary",
];

/// Parse the server's semantic-token legend out of the `initialize` result
/// (`capabilities.semanticTokensProvider.legend`). An absent / boolean provider
/// yields an empty legend, which simply produces zero occurrences (an honest
/// "nothing clickable yet", never a fabricated set).
fn parse_semantic_legend(init: &Value) -> SemanticLegend {
    let legend = init.pointer("/capabilities/semanticTokensProvider/legend");
    let strings = |key: &str| {
        legend
            .and_then(|legend| legend.get(key))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    SemanticLegend {
        token_types: strings("tokenTypes"),
        token_modifiers: strings("tokenModifiers"),
    }
}

/// Whether a semantic-token **type name** denotes a navigable symbol (something
/// `textDocument/definition` can resolve). Keywords, comments, strings,
/// numbers, operators and the like are not clickable identifiers, so excluding
/// them avoids dead clicks (spec: "fail visibly" — no clickable dead spans).
/// rust-analyzer adds custom types (e.g. `builtinType`, `selfKeyword`) that are
/// likewise not navigable. Best-effort and language-agnostic in spirit.
fn is_navigable_token_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "namespace"
            | "module"
            | "type"
            | "typeAlias"
            | "class"
            | "interface"
            | "struct"
            | "enum"
            | "union"
            | "trait"
            | "typeParameter"
            | "parameter"
            | "constParameter"
            | "variable"
            | "property"
            | "field"
            | "enumMember"
            | "function"
            | "method"
            | "macro"
            | "derive"
            | "constant"
            | "static"
            | "decorator"
    )
}

/// Decode a delta-encoded LSP `semanticTokens` result into occurrence ranges.
///
/// The `data` array is groups of five integers
/// `[deltaLine, deltaStartChar, length, tokenType, tokenModifiers]`, each
/// relative to the previous token, in UTF-16 units. We fold the deltas into
/// absolute `(line, startChar)` positions, convert to **byte** ranges via
/// [`LineIndex`] (the only place UTF-16↔byte conversion happens), and keep only
/// navigable token types. `role`/`display` are best-effort from the token type
/// / modifiers and the file text.
fn decode_semantic_occurrences(
    data: &Value,
    legend: &SemanticLegend,
    index: &LineIndex,
    text: &str,
) -> Vec<CodeIntelOccurrence> {
    let Some(items) = data.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut start_char = 0u32;
    for chunk in items.chunks_exact(5) {
        let delta_line = chunk[0].as_u64().unwrap_or(0) as u32;
        let delta_start = chunk[1].as_u64().unwrap_or(0) as u32;
        let length = chunk[2].as_u64().unwrap_or(0) as u32;
        let token_type = chunk[3].as_u64().unwrap_or(0) as usize;
        let modifiers = chunk[4].as_u64().unwrap_or(0);

        if delta_line == 0 {
            start_char = start_char.saturating_add(delta_start);
        } else {
            line = line.saturating_add(delta_line);
            start_char = delta_start;
        }
        if length == 0 {
            continue;
        }
        let type_name = legend
            .token_types
            .get(token_type)
            .map(String::as_str)
            .unwrap_or("");
        if !is_navigable_token_type(type_name) {
            continue;
        }
        let end_char = start_char.saturating_add(length);
        let range = index.range_to_byte_range(line, start_char, line, end_char);
        let display = text
            .get(range.start as usize..range.end as usize)
            .unwrap_or("")
            .to_owned();
        let role = if token_has_definition_modifier(modifiers, legend) {
            CodeIntelRole::Definition
        } else {
            CodeIntelRole::Reference
        };
        out.push(CodeIntelOccurrence {
            range,
            role,
            display,
            definition: Vec::new(),
        });
    }
    out
}

/// Whether a token's modifier bitset has the `definition` or `declaration`
/// modifier set, mapping role best-effort. The bit at index `i` corresponds to
/// `legend.token_modifiers[i]`.
fn token_has_definition_modifier(modifiers: u64, legend: &SemanticLegend) -> bool {
    legend
        .token_modifiers
        .iter()
        .enumerate()
        .any(|(bit, name)| {
            // A server advertising ≥64 modifiers would overflow a `1 << bit`
            // shift; `checked_shl` yields `None` past bit 63, so those bits are
            // simply treated as unset rather than panicking.
            let mask = 1u64.checked_shl(bit as u32).unwrap_or(0);
            (modifiers & mask) != 0 && (name == "definition" || name == "declaration")
        })
}

/// The detached driver that pushes one file's whole-file semantic model (M3).
///
/// Lifecycle (mirrors the syntax-highlight worker's async fill):
///
/// 1. `textDocument/semanticTokens/full` → decode occurrence ranges → emit the
///    clickable identifier set with every `definition` empty (so a click can
///    tell "still resolving" from "no such identifier"). A **small** file emits
///    one `FullFile` `Partial` frame (M3). A **large** file emits the set as
///    transient `ByteRange` `Partial` chunks, the chunk covering the visible
///    window first (M6) — every chunk is delivered, so the whole file is still
///    covered.
/// 2. Resolve `textDocument/definition` per occurrence with bounded concurrency
///    (tightened for large files / a constrained host), **visible range first**
///    (reprioritized live via `visible_rx`). Coalesce resolved occurrences into
///    batched frames (the client merges them by range): `FullFile` for small
///    files, the batch's bounding `ByteRange` for large ones.
/// 3. Emit a final `FullFile` + `Complete` frame. For a large file this is the
///    convergence point — the accumulated `ByteRange` coverage flips to
///    whole-file `Complete`, so `ByteRange` was only a transient pacing window,
///    never a permanent scope gate.
///
/// Cancellation is `cancel_rx` resolving (the actor dropped the handle on
/// unsubscribe or version supersession): the driver returns and emits nothing
/// further — including for the chunked large-file path. Every frame is stamped
/// with the file's `version`.
struct ModelJob {
    requester: LspRequester,
    root: ProjectRootPath,
    path: ProjectPath,
    version: ProjectFileVersion,
    uri: String,
    text: String,
    provider: CodeIntelProviderId,
    language: CodeIntelLanguageId,
    output: Stream,
    legend: SemanticLegend,
    /// Host resource mode (spec §M6): paces large-file delivery (batch / in-flight
    /// caps), never the final whole-file scope.
    resource_mode: CodeIntelResourceMode,
    /// Large-file thresholds + chunk size. Production: [`ModelTuning::DEFAULT`].
    tuning: ModelTuning,
    visible_rx: mpsc::UnboundedReceiver<ByteRange>,
    cancel_rx: oneshot::Receiver<()>,
    model_failed_tx: Option<mpsc::UnboundedSender<RaCommand>>,
}

impl ModelJob {
    async fn run(mut self) {
        // 1. Occurrence set from semanticTokens/full (raced against cancel).
        let params = json!({ "textDocument": { "uri": self.uri } });
        let tokens = tokio::select! {
            _ = &mut self.cancel_rx => return,
            result = self.requester.request("textDocument/semanticTokens/full", params) => result,
        };
        let tokens = match tokens {
            Ok(value) => value,
            Err(error) => {
                let message = format!("semanticTokens/full failed: {error}");
                tracing::debug!(%error, "code-intel: semanticTokens/full failed");
                let code = if error.is_timeout() {
                    CodeIntelErrorCode::Timeout
                } else {
                    CodeIntelErrorCode::Internal
                };
                self.emit_semantic_tokens_failure(code, message);
                return;
            }
        };
        let index = LineIndex::new(&self.text);
        let data = tokens.get("data").cloned().unwrap_or(Value::Null);
        let occurrences = decode_semantic_occurrences(&data, &self.legend, &index, &self.text);

        // Drain any visible-range hints already queued (the client sends one on
        // open; it may land before the occurrence set is decoded) so both the
        // initial chunk order and the resolution queue start visible-first.
        let mut visible: Option<ByteRange> = None;
        while let Ok(range) = self.visible_rx.try_recv() {
            visible = Some(range);
        }

        // §M6: a large file streams its occurrence set as transient `ByteRange`
        // chunks (visible window first); a small file keeps the M3 single
        // `FullFile` frame. Either way every occurrence is delivered.
        let large = is_large_file(self.text.len(), occurrences.len(), &self.tuning);
        if large {
            for chunk in chunk_plan(&occurrences, visible, self.tuning.chunk_occurrences) {
                if let Some(range) = bounding_range(&occurrences, &chunk) {
                    self.emit_subset(
                        &occurrences,
                        &chunk,
                        CodeIntelModelRange::ByteRange { range },
                        CodeIntelCompleteness::Partial,
                    );
                }
            }
        } else {
            self.emit(
                occurrences.clone(),
                CodeIntelModelRange::FullFile,
                CodeIntelCompleteness::Partial,
            );
        }

        // Precompute each occurrence's LSP position once (byte → UTF-16).
        let positions: Vec<(u32, u32)> = occurrences
            .iter()
            .map(|occ| index.byte_to_position(occ.range.start))
            .collect();

        // 2. Incremental definition resolution.
        //
        // The per-occurrence `textDocument/definition` requests run as tasks in a
        // `JoinSet` **owned by this function**, NOT detached `tokio::spawn`s. That
        // is load-bearing for cancellation: when the actor drops this job's
        // `ResolutionHandle` (unsubscribe / version supersession), `cancel_rx`
        // resolves and we `return`; returning drops `tasks`, and a dropped
        // `JoinSet` immediately aborts every task it owns. So no resolution task
        // outlives the job, and repeated cancel/restart can't accumulate zombies.
        //
        // If cancellation aborts a task while its LSP request is in flight, the
        // request future's drop path sends `$/cancelRequest` and removes the
        // pending response entry from the client actor. Late RA responses are
        // then logged as unknown/expired ids and ignored.
        // §M6 resource caps: a large file (or a constrained host) resolves with a
        // tighter in-flight window and smaller flush batches so it can't flood
        // rust-analyzer or the wire. These bound *pace*, never scope.
        let max_inflight = inflight_limit(self.resource_mode, large);
        let batch_cap = batch_limit(self.resource_mode, large);

        let mut occurrences = occurrences;
        let mut queue: VecDeque<usize> = (0..occurrences.len()).collect();
        // Start the resolution queue visible-first too (the chunk emission above
        // already ordered the occurrence-set delivery that way).
        if let Some(range) = visible {
            reprioritize(&mut queue, &occurrences, range);
        }
        let mut tasks: tokio::task::JoinSet<(usize, Vec<CodeIntelLocation>)> =
            tokio::task::JoinSet::new();
        let mut batch: Vec<usize> = Vec::new();
        let mut flush_deadline: Option<tokio::time::Instant> = None;

        loop {
            // Fill the in-flight window from the (possibly reprioritized) queue.
            while tasks.len() < max_inflight {
                let Some(idx) = queue.pop_front() else {
                    break;
                };
                let requester = self.requester.clone();
                let root = self.root.clone();
                let uri = self.uri.clone();
                let (line, character) = positions[idx];
                tasks.spawn(async move {
                    let targets =
                        resolve_definition(&requester, &root, &uri, line, character).await;
                    (idx, targets)
                });
            }
            if tasks.is_empty() && queue.is_empty() {
                break;
            }

            let flush = async {
                match flush_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                // Dropping `tasks` on return aborts every in-flight definition task.
                _ = &mut self.cancel_rx => return,
                visible = self.visible_rx.recv() => {
                    match visible {
                        Some(range) => reprioritize(&mut queue, &occurrences, range),
                        // The handle (which owns `visible_tx` and `_cancel_tx`)
                        // was dropped — same as cancellation. Stop here.
                        None => return,
                    }
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    let (idx, targets) = match joined {
                        Ok(result) => result,
                        // An aborted task (cancellation race) or a panicked one:
                        // skip it rather than crash the resolution loop.
                        Err(error) => {
                            if !error.is_cancelled() {
                                tracing::debug!(%error, "code-intel: definition task failed");
                            }
                            continue;
                        }
                    };
                    occurrences[idx].definition = targets;
                    batch.push(idx);
                    if flush_deadline.is_none() {
                        flush_deadline = Some(tokio::time::Instant::now() + RESOLUTION_FLUSH);
                    }
                    if batch.len() >= batch_cap {
                        self.emit_resolved_batch(&occurrences, &batch, large);
                        batch.clear();
                        flush_deadline = None;
                    }
                }
                _ = flush, if flush_deadline.is_some() => {
                    self.emit_resolved_batch(&occurrences, &batch, large);
                    batch.clear();
                    flush_deadline = None;
                }
            }
        }

        // 3. Final `FullFile` + `Complete` frame: flushes any trailing batch and
        // flips completeness. For a large file this is the convergence point —
        // the accumulated `ByteRange` coverage becomes whole-file `Complete`, so
        // `ByteRange` was only a transient delivery window, never a scope gate.
        self.emit_subset(
            &occurrences,
            &batch,
            CodeIntelModelRange::FullFile,
            CodeIntelCompleteness::Complete,
        );
    }

    fn emit_semantic_tokens_failure(&mut self, code: CodeIntelErrorCode, message: String) {
        let error = CodeIntelErrorPayload {
            code,
            message: message.clone(),
            hint: None,
            exit_status: None,
            stderr: None,
            context: CodeIntelErrorContext::Subscribe {
                path: self.path.clone(),
            },
            fatal: false,
        };
        emit(&self.output, FrameKind::CodeIntelError, &error);
        let status = CodeIntelStatusPayload {
            scope: CodeIntelStatusScope::File {
                path: self.path.clone(),
                version: self.version,
            },
            state: CodeIntelState::Failed,
            resource_mode: self.resource_mode,
            work_done: None,
            total_work: None,
            message: Some(message),
        };
        emit(&self.output, FrameKind::CodeIntelStatus, &status);
        if let Some(tx) = &self.model_failed_tx {
            let _ = tx.send(RaCommand::ModelFailed {
                path: self.path.clone(),
                version: self.version,
            });
        }
    }

    /// Emit one streamed batch of freshly-resolved occurrences as a `Partial`
    /// frame. A small file advertises `FullFile` (M3); a large file advertises
    /// the batch's bounding `ByteRange` (the transient delivery window, §M6). An
    /// empty batch emits nothing.
    fn emit_resolved_batch(
        &self,
        occurrences: &[CodeIntelOccurrence],
        batch: &[usize],
        large: bool,
    ) {
        if batch.is_empty() {
            return;
        }
        let model_range = match large.then(|| bounding_range(occurrences, batch)).flatten() {
            Some(range) => CodeIntelModelRange::ByteRange { range },
            None => CodeIntelModelRange::FullFile,
        };
        self.emit_subset(
            occurrences,
            batch,
            model_range,
            CodeIntelCompleteness::Partial,
        );
    }

    fn emit(
        &self,
        occurrences: Vec<CodeIntelOccurrence>,
        model_range: CodeIntelModelRange,
        completeness: CodeIntelCompleteness,
    ) {
        let payload = CodeIntelFileModelPayload {
            path: self.path.clone(),
            version: self.version,
            provider: self.provider.clone(),
            language: self.language.clone(),
            model_range,
            completeness,
            occurrences,
        };
        emit(&self.output, FrameKind::CodeIntelFileModel, &payload);
    }

    fn emit_subset(
        &self,
        occurrences: &[CodeIntelOccurrence],
        indices: &[usize],
        model_range: CodeIntelModelRange,
        completeness: CodeIntelCompleteness,
    ) {
        let subset = indices.iter().map(|&i| occurrences[i].clone()).collect();
        self.emit(subset, model_range, completeness);
    }
}

/// Resolve one occurrence's definition target(s), converting LSP locations to
/// in-project byte-offset locations. Reuses [`locations_to_byte_targets`], so
/// out-of-root targets are filtered exactly as the on-demand M2 path does. A
/// failed request is an honest empty answer, never a fabricated target.
async fn resolve_definition(
    requester: &LspRequester,
    root: &ProjectRootPath,
    uri: &str,
    line: u32,
    character: u32,
) -> Vec<CodeIntelLocation> {
    let params = json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    });
    match requester.request("textDocument/definition", params).await {
        Ok(value) => locations_to_byte_targets(root, value).await,
        Err(error) => {
            tracing::debug!(%error, "code-intel: push definition resolve failed");
            Vec::new()
        }
    }
}

/// Move occurrences intersecting the visible byte range to the front of the
/// resolution queue (stable within each partition), so on-screen identifiers
/// resolve first. Pure prioritization — every occurrence still resolves.
fn reprioritize(
    queue: &mut VecDeque<usize>,
    occurrences: &[CodeIntelOccurrence],
    visible: ByteRange,
) {
    let intersects = |range: ByteRange| range.start < visible.end && range.end > visible.start;
    let mut front: Vec<usize> = Vec::new();
    let mut back: Vec<usize> = Vec::new();
    for &idx in queue.iter() {
        if intersects(occurrences[idx].range) {
            front.push(idx);
        } else {
            back.push(idx);
        }
    }
    queue.clear();
    queue.extend(front);
    queue.extend(back);
}

enum ActiveIdRequestOutcome {
    Response(Result<Value, LspError>),
    Superseded,
}

async fn request_while_active_id(
    requester: &LspRequester,
    method: &str,
    params: Value,
    active: &AtomicU64,
    request_id: u64,
) -> ActiveIdRequestOutcome {
    if active.load(Ordering::SeqCst) != request_id {
        return ActiveIdRequestOutcome::Superseded;
    }
    let pending = match requester.start_request(method, params).await {
        Ok(pending) => pending,
        Err(error) => return ActiveIdRequestOutcome::Response(Err(error)),
    };
    let lsp_id = pending.id();
    let mut response = Box::pin(pending.response());
    loop {
        tokio::select! {
            result = &mut response => {
                return ActiveIdRequestOutcome::Response(result);
            }
            _ = tokio::time::sleep(REQUEST_CANCEL_POLL) => {
                if active.load(Ordering::SeqCst) != request_id {
                    if let Err(error) = requester.cancel_request(lsp_id) {
                        tracing::debug!(%error, lsp_id, "code-intel: failed to cancel superseded LSP request");
                    }
                    return ActiveIdRequestOutcome::Superseded;
                }
            }
        }
    }
}

/// Whether an in-flight find-references query should keep running, be reported
/// as cancelled, or stay silent because a newer query superseded it. Decided
/// purely from the active-id atomic vs. the query's own id (mirrors the
/// `search_project_files` supersede/cancel split): equal ⇒ live; `0` ⇒ the id
/// was reset by an explicit cancel; any other value ⇒ a newer query owns the
/// stream now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferencesProgress {
    Live,
    Cancelled,
    Superseded,
}

enum ReferencesRequestOutcome {
    Response(Result<Value, LspError>),
    Cancelled,
    Superseded,
}

type LspRangeTuple = (u32, u32, u32, u32);
type ReferencesByFile = Vec<(String, Vec<LspRangeTuple>)>;

fn references_progress(active: u64, references_id: u64) -> ReferencesProgress {
    if active == references_id {
        ReferencesProgress::Live
    } else if active == 0 {
        ReferencesProgress::Cancelled
    } else {
        ReferencesProgress::Superseded
    }
}

/// The detached driver for one find-references query (M5). It issues a single
/// `textDocument/references`, groups the returned locations by file, converts
/// each to byte ranges in the *target* file's coordinates (filtering
/// out-of-root hits exactly like the M2 definition path), builds per-line
/// previews, and streams one `code_intel_references_results` per file followed
/// by a terminal `code_intel_references_complete`. Every step re-checks the
/// active-id atomic so a newer query (drop late frames) or an explicit cancel
/// (terminate with `cancelled: true`) takes effect promptly.
struct FindReferencesJob {
    requester: LspRequester,
    root: ProjectRootPath,
    references_id: u64,
    path: ProjectPath,
    /// Source file URI + text, for the byte→UTF-16 conversion of `offset`.
    uri: String,
    text: String,
    offset: u32,
    include_declaration: bool,
    output: Stream,
    active: Arc<AtomicU64>,
}

impl FindReferencesJob {
    async fn run(self) {
        let index = LineIndex::new(&self.text);
        let (line, character) = index.byte_to_position(self.offset);
        let params = json!({
            "textDocument": { "uri": self.uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": self.include_declaration },
        });
        let response = match self.request_while_live(params).await {
            ReferencesRequestOutcome::Response(response) => response,
            ReferencesRequestOutcome::Cancelled => return self.emit_cancelled(),
            ReferencesRequestOutcome::Superseded => return,
        };

        let locations = match response {
            Ok(value) => parse_lsp_locations(&value),
            Err(error) => {
                tracing::debug!(%error, "code-intel: textDocument/references failed");
                if error.is_timeout() {
                    emit_query_error(
                        &self.output,
                        CodeIntelErrorCode::Timeout,
                        format!("textDocument/references timed out: {error}"),
                        CodeIntelErrorContext::FindReferences {
                            references_id: self.references_id,
                            path: self.path.clone(),
                        },
                    );
                }
                self.emit_complete(
                    0,
                    0,
                    false,
                    false,
                    Some(format!("find-references failed: {error}")),
                );
                return;
            }
        };

        // Group by file URI, sorted for deterministic streaming order.
        let mut by_file: HashMap<String, Vec<LspRangeTuple>> = HashMap::new();
        for (uri, range) in locations {
            by_file.entry(uri).or_default().push(range);
        }
        let mut files: ReferencesByFile = by_file.into_iter().collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));

        let mut total_files = 0u32;
        let mut total_references = 0u32;
        let mut any_truncated = false;
        for (uri, ranges) in files {
            match references_progress(self.active.load(Ordering::SeqCst), self.references_id) {
                ReferencesProgress::Live => {}
                ReferencesProgress::Cancelled => return self.emit_cancelled(),
                ReferencesProgress::Superseded => return,
            }
            let Some(absolute) = uri_to_path(&uri) else {
                continue;
            };
            let Some(relative_path) = relative_to_root(&self.root, &absolute) else {
                // Outside the project root (stdlib, registry deps): not navigable.
                continue;
            };
            let file_text = match tokio::fs::read_to_string(&absolute).await {
                Ok(text) => text,
                Err(error) => {
                    tracing::debug!(%error, ?absolute, "code-intel: failed to read references target");
                    continue;
                }
            };
            let file_index = LineIndex::new(&file_text);
            let (lines, file_refs, truncated) = build_reference_lines(&file_index, &ranges);
            if lines.is_empty() {
                continue;
            }
            total_files += 1;
            total_references += file_refs;
            any_truncated |= truncated;

            // Re-check immediately before sending: a long disk read may have
            // outlived a cancel/supersede since the per-file poll above.
            match references_progress(self.active.load(Ordering::SeqCst), self.references_id) {
                ReferencesProgress::Live => {}
                ReferencesProgress::Cancelled => return self.emit_cancelled(),
                ReferencesProgress::Superseded => return,
            }
            let payload = CodeIntelReferencesResultsPayload {
                references_id: self.references_id,
                file: CodeIntelReferencesFileResult {
                    path: ProjectPath {
                        root: self.root.clone(),
                        relative_path,
                    },
                    lines,
                    truncated,
                },
            };
            emit(
                &self.output,
                FrameKind::CodeIntelReferencesResults,
                &payload,
            );
        }

        match references_progress(self.active.load(Ordering::SeqCst), self.references_id) {
            ReferencesProgress::Live => {
                self.emit_complete(total_files, total_references, any_truncated, false, None)
            }
            ReferencesProgress::Cancelled => self.emit_cancelled(),
            ReferencesProgress::Superseded => {}
        }
    }

    fn emit_complete(
        &self,
        total_files: u32,
        total_references: u32,
        truncated: bool,
        cancelled: bool,
        error: Option<String>,
    ) {
        let complete = CodeIntelReferencesCompletePayload {
            references_id: self.references_id,
            total_files,
            total_references,
            truncated,
            cancelled,
            error,
        };
        emit(
            &self.output,
            FrameKind::CodeIntelReferencesComplete,
            &complete,
        );
    }

    fn emit_cancelled(&self) {
        self.emit_complete(0, 0, false, true, None);
    }

    async fn request_while_live(&self, params: Value) -> ReferencesRequestOutcome {
        match references_progress(self.active.load(Ordering::SeqCst), self.references_id) {
            ReferencesProgress::Live => {}
            ReferencesProgress::Cancelled => return ReferencesRequestOutcome::Cancelled,
            ReferencesProgress::Superseded => return ReferencesRequestOutcome::Superseded,
        }
        let pending = match self
            .requester
            .start_request("textDocument/references", params)
            .await
        {
            Ok(pending) => pending,
            Err(error) => return ReferencesRequestOutcome::Response(Err(error)),
        };
        let lsp_id = pending.id();
        let mut response = Box::pin(pending.response());
        loop {
            tokio::select! {
                result = &mut response => {
                    return ReferencesRequestOutcome::Response(result);
                }
                _ = tokio::time::sleep(REQUEST_CANCEL_POLL) => {
                    match references_progress(self.active.load(Ordering::SeqCst), self.references_id) {
                        ReferencesProgress::Live => {}
                        ReferencesProgress::Cancelled => {
                            if let Err(error) = self.requester.cancel_request(lsp_id) {
                                tracing::debug!(%error, lsp_id, "code-intel: failed to cancel find-references LSP request");
                            }
                            return ReferencesRequestOutcome::Cancelled;
                        }
                        ReferencesProgress::Superseded => {
                            if let Err(error) = self.requester.cancel_request(lsp_id) {
                                tracing::debug!(%error, lsp_id, "code-intel: failed to cancel superseded find-references LSP request");
                            }
                            return ReferencesRequestOutcome::Superseded;
                        }
                    }
                }
            }
        }
    }
}

/// Convert one file's reference locations (UTF-16 LSP ranges into that file) to
/// per-line previews with line-relative byte ranges. Locations are grouped by
/// 1-based line; the line text is sent verbatim (trailing `\r` trimmed) and the
/// ranges are byte offsets into it. Returns the lines (sorted by line number),
/// the reference count, and whether the per-file cap was hit. A reference that
/// spans lines is clamped to its start line's content.
fn build_reference_lines(
    index: &LineIndex,
    ranges: &[(u32, u32, u32, u32)],
) -> (Vec<CodeIntelReferenceLine>, u32, bool) {
    let mut by_line: BTreeMap<u32, Vec<ByteRange>> = BTreeMap::new();
    let mut count = 0usize;
    let mut truncated = false;
    for &(start_line, start_char, end_line, end_char) in ranges {
        if count >= MAX_REFERENCES_PER_FILE {
            truncated = true;
            break;
        }
        let absolute = index.range_to_byte_range(start_line, start_char, end_line, end_char);
        let Some((line_start, line_text)) = index.line_span(start_line) else {
            continue;
        };
        let line_end = line_start + line_text.len() as u32;
        let start = absolute.start.saturating_sub(line_start);
        let end = absolute.end.min(line_end).saturating_sub(line_start);
        by_line
            .entry(start_line)
            .or_default()
            .push(ByteRange { start, end });
        count += 1;
    }
    let lines = by_line
        .into_iter()
        .map(|(line, mut ranges)| {
            ranges.sort_by_key(|range| range.start);
            let line_text = index.line_span(line).map(|(_, text)| text).unwrap_or("");
            CodeIntelReferenceLine {
                line_number: line + 1,
                line_text: line_text.trim_end_matches('\r').to_owned(),
                ranges,
            }
        })
        .collect();
    (lines, count as u32, truncated)
}

/// Map an absolute filesystem path to a canonical `file://` URI via `url`, so
/// spaces / non-ASCII are percent-encoded exactly the way rust-analyzer
/// normalizes them — otherwise a raw URI would never equal RA's in
/// `publishDiagnostics`. `None` if the path isn't absolute (`from_file_path`
/// requires it; project roots always are).
fn file_uri(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

/// Decode a `file://` URI back to a filesystem path for diagnostic matching.
/// `None` for a non-file or unparseable URI.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    Url::parse(uri).ok()?.to_file_path().ok()
}

/// Extract a 0–100 work amount from a `$/progress` value, mapping `percentage`
/// to `work_done` out of a `total_work` of 100.
fn progress_amounts(value: &Value) -> (Option<u32>, Option<u32>) {
    match value.get("percentage").and_then(Value::as_u64) {
        Some(percentage) => (Some(percentage.min(100) as u32), Some(100)),
        None => (None, None),
    }
}

/// Convert one LSP diagnostic JSON object into a Tyde [`CodeIntelDiagnostic`]
/// with byte-offset ranges. Returns `None` for a malformed entry (missing
/// range) rather than fabricating a position.
fn convert_diagnostic(index: &LineIndex, diagnostic: &Value) -> Option<CodeIntelDiagnostic> {
    let range = diagnostic.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_line = start.get("line")?.as_u64()? as u32;
    let start_char = start.get("character")?.as_u64()? as u32;
    let end_line = end.get("line")?.as_u64()? as u32;
    let end_char = end.get("character")?.as_u64()? as u32;

    let byte_range = index.range_to_byte_range(start_line, start_char, end_line, end_char);
    let severity = match diagnostic.get("severity").and_then(Value::as_u64) {
        Some(1) => CodeIntelSeverity::Error,
        Some(2) => CodeIntelSeverity::Warning,
        Some(3) => CodeIntelSeverity::Information,
        Some(4) => CodeIntelSeverity::Hint,
        // LSP leaves severity optional; default to Warning rather than guessing
        // Error so an un-annotated lint doesn't read as a hard failure.
        _ => CodeIntelSeverity::Warning,
    };
    let message = diagnostic
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let source = diagnostic
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_owned);

    Some(CodeIntelDiagnostic {
        range: byte_range,
        severity,
        message,
        source,
    })
}

/// Convert an LSP `textDocument/definition` result into Tyde byte-offset
/// locations. The result is `Location | Location[] | LocationLink[] | null`.
/// Each target's range is in the **target file's** UTF-16 coordinates, so we
/// read that file and build a fresh [`LineIndex`] per target to convert it to
/// bytes — the conversion is confined here, never exposed to the frontend.
///
/// Targets outside the project root (stdlib, cargo-registry deps) are skipped
/// in M2: a `ProjectPath` is rooted at the project, and the frontend can only
/// open in-project files. Cross-crate-out-of-root navigation is deferred.
async fn locations_to_byte_targets(root: &ProjectRootPath, value: Value) -> Vec<CodeIntelLocation> {
    let raw = parse_lsp_locations(&value);
    let mut out = Vec::new();
    for (uri, (start_line, start_char, end_line, end_char)) in raw {
        let Some(absolute) = uri_to_path(&uri) else {
            continue;
        };
        let Some(relative_path) = relative_to_root(root, &absolute) else {
            // Outside the project root: not navigable in M2.
            continue;
        };
        let text = match tokio::fs::read_to_string(&absolute).await {
            Ok(text) => text,
            Err(error) => {
                tracing::debug!(%error, ?absolute, "code-intel: failed to read definition target");
                continue;
            }
        };
        let index = LineIndex::new(&text);
        let range = index.range_to_byte_range(start_line, start_char, end_line, end_char);
        out.push(CodeIntelLocation {
            path: ProjectPath {
                root: root.clone(),
                relative_path,
            },
            range,
        });
    }
    out
}

/// Flatten an LSP definition result into `(uri, (start_line, start_char,
/// end_line, end_char))` tuples, handling all three shapes (single `Location`,
/// array of `Location`, array of `LocationLink`).
fn parse_lsp_locations(value: &Value) -> Vec<(String, (u32, u32, u32, u32))> {
    match value {
        Value::Array(items) => items.iter().filter_map(parse_one_location).collect(),
        Value::Null => Vec::new(),
        single => parse_one_location(single).into_iter().collect(),
    }
}

/// Parse one `Location` (`{uri, range}`) or `LocationLink`
/// (`{targetUri, targetSelectionRange|targetRange}`) object.
fn parse_one_location(value: &Value) -> Option<(String, (u32, u32, u32, u32))> {
    if let Some(uri) = value.get("uri").and_then(Value::as_str) {
        let range = parse_lsp_range(value.get("range")?)?;
        return Some((uri.to_owned(), range));
    }
    if let Some(uri) = value.get("targetUri").and_then(Value::as_str) {
        // Prefer the precise selection range (the identifier) over the full
        // target range (the whole item) when both are present.
        let range_value = value
            .get("targetSelectionRange")
            .or_else(|| value.get("targetRange"))?;
        let range = parse_lsp_range(range_value)?;
        return Some((uri.to_owned(), range));
    }
    None
}

/// Parse an LSP `Range` (`{start:{line,character}, end:{line,character}}`) into
/// `(start_line, start_char, end_line, end_char)`.
fn parse_lsp_range(range: &Value) -> Option<(u32, u32, u32, u32)> {
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some((
        start.get("line")?.as_u64()? as u32,
        start.get("character")?.as_u64()? as u32,
        end.get("line")?.as_u64()? as u32,
        end.get("character")?.as_u64()? as u32,
    ))
}

/// Convert an LSP `textDocument/hover` result into `(markdown, byte_range)`.
/// `contents` is `MarkupContent | MarkedString | MarkedString[]`; the optional
/// `range` is in the **hovered file's** UTF-16 coordinates, converted with the
/// source file's `index`. `None` markdown means "nothing to show here".
fn convert_hover(index: &LineIndex, value: &Value) -> (Option<String>, Option<ByteRange>) {
    if value.is_null() {
        return (None, None);
    }
    let contents = value.get("contents").map(hover_contents_to_markdown);
    let markdown = match contents {
        Some(text) if !text.trim().is_empty() => Some(text),
        _ => None,
    };
    let range = value
        .get("range")
        .and_then(parse_lsp_range)
        .map(|(sl, sc, el, ec)| index.range_to_byte_range(sl, sc, el, ec));
    (markdown, range)
}

/// Render LSP hover `contents` to a single markdown string. A bare
/// `MarkedString` string is used verbatim; a `{language, value}` MarkedString
/// becomes a fenced code block; an array is joined with blank lines.
fn hover_contents_to_markdown(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(hover_contents_to_markdown)
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => {
            // MarkupContent `{kind, value}` (value is already markdown) or a
            // MarkedString `{language, value}` (wrap in a fenced block).
            let value = map.get("value").and_then(Value::as_str).unwrap_or_default();
            match map.get("language").and_then(Value::as_str) {
                Some(language) => format!("```{language}\n{value}\n```"),
                None => value.to_owned(),
            }
        }
        _ => String::new(),
    }
}

/// The relative path of `absolute` within `root`, or `None` if it isn't under
/// the root. Used to map an LSP definition target's filesystem path back to an
/// in-project [`ProjectPath`].
fn relative_to_root(root: &ProjectRootPath, absolute: &Path) -> Option<String> {
    // Deliberately lexical: URI decoding has already rejected escaping paths;
    // symlink canonicalization is left to the trusted host language server.
    absolute
        .strip_prefix(&root.0)
        .ok()
        .map(|relative| relative.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::StreamPath;
    use std::time::Duration;

    /// A rust-shaped [`LanguageServerConfig`] for engine tests. Discovery is a
    /// stub (these tests inject a fake LSP directly and never call `start()`), so
    /// the provider/language ids and lsp languageId are what matter.
    fn test_config() -> LanguageServerConfig {
        LanguageServerConfig {
            language: CodeIntelLanguageId("rust".to_owned()),
            provider_id: CodeIntelProviderId("rust-analyzer".to_owned()),
            lsp_language_id: "rust",
            extensions: &["rs"],
            workspace_markers: &["Cargo.toml"],
            discover: |_, _| ServerDiscovery::absent_install("rust-analyzer", "test: no binary"),
            configured_path: None,
            initialization_options: || json!({}),
        }
    }

    #[cfg(unix)]
    static TEST_CRASHING_LSP: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

    #[cfg(unix)]
    fn discover_test_crashing_lsp(
        _: &Path,
        _configured_path: Option<&protocol::HostExecutablePath>,
    ) -> ServerDiscovery {
        let binary = TEST_CRASHING_LSP
            .lock()
            .expect("test crashing LSP mutex poisoned")
            .clone()
            .expect("test crashing LSP path set");
        ServerDiscovery::Found {
            binary,
            args: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn crashing_test_config() -> LanguageServerConfig {
        LanguageServerConfig {
            language: CodeIntelLanguageId("rust".to_owned()),
            provider_id: CodeIntelProviderId("rust-analyzer".to_owned()),
            lsp_language_id: "rust",
            extensions: &["rs"],
            workspace_markers: &["Cargo.toml"],
            discover: discover_test_crashing_lsp,
            configured_path: None,
            initialization_options: || json!({}),
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).expect("write executable");
        let mut permissions = std::fs::metadata(path)
            .expect("stat executable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod executable");
    }

    /// An actor over [`test_config`] with a throwaway self-sender (the restart
    /// scheduler is exercised explicitly where needed).
    fn test_actor(root: ProjectRootPath, resource_mode: CodeIntelResourceMode) -> RaActor {
        let (tx, _rx) = mpsc::unbounded_channel();
        RaActor::new(test_config(), root, resource_mode, tx, None)
    }

    fn test_actor_with_status_tx(
        root: ProjectRootPath,
        resource_mode: CodeIntelResourceMode,
        provider_status_tx: mpsc::UnboundedSender<CodeIntelProviderStatus>,
    ) -> RaActor {
        let (tx, _rx) = mpsc::unbounded_channel();
        RaActor::new(
            test_config(),
            root,
            resource_mode,
            tx,
            Some(provider_status_tx),
        )
    }

    fn add_subscribed_file(
        actor: &mut RaActor,
        relative_path: &str,
    ) -> (ProjectPath, mpsc::UnboundedReceiver<protocol::Envelope>) {
        let path = ProjectPath {
            root: actor.root.clone(),
            relative_path: relative_path.to_owned(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        actor.files.insert(
            path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: Arc::new(AtomicU64::new(1)),
                output: stream,
                text: String::new(),
                absolute: absolute_path(&path),
                model_version: None,
            },
        );
        (path, rx)
    }

    fn drain_status_frames(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
    ) -> Vec<CodeIntelStatusPayload> {
        let mut statuses = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            if envelope.kind == FrameKind::CodeIntelStatus {
                statuses.push(serde_json::from_value(envelope.payload).expect("status payload"));
            }
        }
        statuses
    }

    fn drain_provider_statuses(
        rx: &mut mpsc::UnboundedReceiver<CodeIntelProviderStatus>,
    ) -> Vec<CodeIntelProviderStatus> {
        let mut statuses = Vec::new();
        while let Ok(status) = rx.try_recv() {
            statuses.push(status);
        }
        statuses
    }

    #[test]
    fn provider_status_updates_include_provider_language_and_progress() {
        let (status_tx, mut status_rx) = mpsc::unbounded_channel();
        let mut actor = test_actor_with_status_tx(
            ProjectRootPath("/repo".to_owned()),
            CodeIntelResourceMode::Limited,
            status_tx,
        );

        actor.emit_status_all(Some(25), Some(100), true);

        let status = status_rx.try_recv().expect("provider status update");
        assert_eq!(
            status.provider,
            CodeIntelProviderId("rust-analyzer".to_owned())
        );
        assert_eq!(status.language, CodeIntelLanguageId("rust".to_owned()));
        assert_eq!(status.state, CodeIntelState::Starting);
        assert_eq!(status.resource_mode, CodeIntelResourceMode::Limited);
        assert_eq!(status.work_done, Some(25));
        assert_eq!(status.total_work, Some(100));
    }

    #[test]
    fn repeated_identical_status_is_deduped() {
        let (status_tx, mut status_rx) = mpsc::unbounded_channel();
        let mut actor = test_actor_with_status_tx(
            ProjectRootPath("/repo".to_owned()),
            CodeIntelResourceMode::Full,
            status_tx,
        );
        let (_path, mut file_rx) = add_subscribed_file(&mut actor, "src/main.rs");

        actor.emit_status_all(Some(1), Some(10), true);
        actor.emit_status_all(Some(1), Some(10), true);
        actor.emit_status_all(Some(1), Some(10), true);

        assert_eq!(
            drain_provider_statuses(&mut status_rx).len(),
            1,
            "provider overview status should drop exact repeats"
        );
        assert_eq!(
            drain_status_frames(&mut file_rx).len(),
            1,
            "file status should drop exact repeats"
        );
    }

    #[test]
    fn progress_reports_are_coalesced_but_phase_changes_emit() {
        let mut actor = test_actor(
            ProjectRootPath("/repo".to_owned()),
            CodeIntelResourceMode::Full,
        );
        let (_path, mut file_rx) = add_subscribed_file(&mut actor, "src/main.rs");
        actor.phase = Phase::Indexing;

        actor.emit_status_all(Some(0), Some(100), false);
        let _ = drain_status_frames(&mut file_rx);
        for done in 1..=100 {
            actor.on_progress(json!({
                "token": "index",
                "value": {
                    "kind": "report",
                    "percentage": done
                }
            }));
        }
        let coalesced = drain_status_frames(&mut file_rx);
        assert!(
            coalesced.len() < 100,
            "progress report storm should be throttled before emission"
        );

        actor.set_phase(Phase::Ready, None);
        let ready = drain_status_frames(&mut file_rx);
        assert_eq!(ready.len(), 1, "phase transition emits immediately");
        assert_eq!(ready[0].state, CodeIntelState::Ready);

        actor.set_phase(Phase::Ready, None);
        assert!(
            drain_status_frames(&mut file_rx).is_empty(),
            "unchanged phase/status is still deduped"
        );
    }

    #[test]
    fn converts_lsp_diagnostic_to_byte_range() {
        let text = "fn main() {\n    let x = 名前;\n}\n";
        let index = LineIndex::new(text);
        // Line 1 (0-based): the prefix "    let x = " is 12 UTF-16 units, so the
        // CJK "名前" occupies characters 12..14.
        let diagnostic = json!({
            "range": {
                "start": {"line": 1, "character": 12},
                "end": {"line": 1, "character": 14},
            },
            "severity": 1,
            "message": "cannot find value `名前`",
            "source": "rustc",
        });
        let converted = convert_diagnostic(&index, &diagnostic).expect("valid diagnostic");
        assert_eq!(converted.severity, CodeIntelSeverity::Error);
        assert_eq!(converted.source.as_deref(), Some("rustc"));
        // "    let x = " is 12 bytes on line 1; line 1 starts at byte 12.
        // 12 (line start) + 12 (prefix) = 24, and "名前" is 6 bytes → 24..30.
        assert_eq!(converted.range, ByteRange { start: 24, end: 30 });
    }

    #[test]
    fn malformed_diagnostic_is_skipped_not_fabricated() {
        let index = LineIndex::new("x");
        let diagnostic = json!({"severity": 1, "message": "no range here"});
        assert!(convert_diagnostic(&index, &diagnostic).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn file_uri_round_trips_with_space_and_non_ascii() {
        // A path with a space and non-ASCII must be percent-encoded the way RA
        // normalizes it, and must decode back to the exact same path.
        let path = std::path::PathBuf::from("/tmp/my project/café.rs");
        let uri = file_uri(&path).expect("absolute path yields a URI");
        assert!(uri.contains("%20"), "space must be percent-encoded: {uri}");
        assert!(
            !uri.contains("café"),
            "non-ASCII must be percent-encoded: {uri}"
        );
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_uri_decodes_back_to_stored_path() {
        // Simulates RA returning a normalized URI: decoding it must match the
        // path we stored, so the diagnostic isn't silently dropped.
        let path = std::path::PathBuf::from("/tmp/space dir/файл.rs");
        let normalized = file_uri(&path).unwrap();
        assert_eq!(uri_to_path(&normalized).unwrap(), path);
    }

    /// A closed stream: emitting on it is a harmless no-op. Used as the initial
    /// output so the test can prove the *re-subscribe* stream is the one that
    /// receives diagnostics.
    fn dead_stream() -> Stream {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        Stream::new(StreamPath("/project/dead".to_owned()), tx)
    }

    #[tokio::test]
    async fn resubscribe_preserves_didopen_text_and_still_delivers_diagnostics() {
        // Reproduces BLOCKER 2: re-subscribing an already-open file must not
        // clobber the stored didOpen text/path, so a later publishDiagnostics
        // still matches and is delivered (now onto the new output stream).
        let root = ProjectRootPath("/repo".to_owned());
        let mut actor = test_actor(root.clone(), CodeIntelResourceMode::Full);
        actor.phase = Phase::Ready;

        let path = ProjectPath {
            root: root.clone(),
            relative_path: "src/main.rs".to_owned(),
        };
        let absolute = absolute_path(&path);
        let text = "fn main() { let _x: i32 = \"no\"; }".to_owned();
        actor.files.insert(
            path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: Arc::new(AtomicU64::new(1)),
                output: dead_stream(),
                text: text.clone(),
                absolute: absolute.clone(),
                model_version: None,
            },
        );
        actor.opened.insert(path.clone());

        // Re-subscribe with a fresh output stream.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        actor
            .handle_command(RaCommand::Subscribe {
                path: path.clone(),
                version: ProjectFileVersion(1),
                output: stream,
            })
            .await;

        // The didOpen text/path survived the re-subscribe.
        let file = actor.files.get(&path).expect("file still tracked");
        assert_eq!(
            file.text, text,
            "re-subscribe must not clobber didOpen text"
        );
        assert_eq!(file.absolute, absolute);

        // A publishDiagnostics for that file (by normalized URI) is delivered.
        let uri = file_uri(&absolute).unwrap();
        actor.on_publish_diagnostics(json!({
            "uri": uri,
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
                "severity": 1,
                "message": "boom",
            }],
        }));

        let mut delivered = None;
        while let Ok(envelope) = rx.try_recv() {
            if envelope.kind == FrameKind::CodeIntelDiagnostics {
                delivered = Some(
                    serde_json::from_value::<CodeIntelDiagnosticsPayload>(envelope.payload)
                        .unwrap(),
                );
            }
        }
        let payload = delivered.expect("diagnostics delivered on the re-subscribe stream");
        assert_eq!(payload.diagnostics.len(), 1);
        assert_eq!(payload.path, path);
    }

    #[test]
    fn severity_maps_each_lsp_level() {
        let index = LineIndex::new("abc");
        let make = |sev: u64| {
            json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "severity": sev,
                "message": "m",
            })
        };
        assert_eq!(
            convert_diagnostic(&index, &make(1)).unwrap().severity,
            CodeIntelSeverity::Error
        );
        assert_eq!(
            convert_diagnostic(&index, &make(2)).unwrap().severity,
            CodeIntelSeverity::Warning
        );
        assert_eq!(
            convert_diagnostic(&index, &make(3)).unwrap().severity,
            CodeIntelSeverity::Information
        );
        assert_eq!(
            convert_diagnostic(&index, &make(4)).unwrap().severity,
            CodeIntelSeverity::Hint
        );
    }

    // ── Pure LSP-result parsing (no subprocess) ────────────────────────────

    #[test]
    fn parse_single_location_and_array_and_link_shapes() {
        let single = json!({
            "uri": "file:///x.rs",
            "range": {"start": {"line": 1, "character": 2}, "end": {"line": 3, "character": 4}},
        });
        assert_eq!(
            parse_lsp_locations(&single),
            vec![("file:///x.rs".to_owned(), (1, 2, 3, 4))]
        );

        let array = json!([single, {
            "uri": "file:///y.rs",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        }]);
        assert_eq!(parse_lsp_locations(&array).len(), 2);

        // LocationLink prefers targetSelectionRange over targetRange.
        let link = json!([{
            "targetUri": "file:///z.rs",
            "targetRange": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 9}},
            "targetSelectionRange": {"start": {"line": 9, "character": 4}, "end": {"line": 9, "character": 7}},
        }]);
        assert_eq!(
            parse_lsp_locations(&link),
            vec![("file:///z.rs".to_owned(), (9, 4, 9, 7))]
        );

        assert!(parse_lsp_locations(&Value::Null).is_empty());
    }

    #[test]
    fn hover_contents_markup_marked_and_array() {
        // MarkupContent: value is already markdown.
        let markup = json!({"kind": "markdown", "value": "`fn helper()`"});
        assert_eq!(hover_contents_to_markdown(&markup), "`fn helper()`");
        // MarkedString object: wrap in a fenced block.
        let marked = json!({"language": "rust", "value": "fn helper()"});
        assert_eq!(
            hover_contents_to_markdown(&marked),
            "```rust\nfn helper()\n```"
        );
        // Bare string MarkedString: verbatim.
        assert_eq!(hover_contents_to_markdown(&json!("plain")), "plain");
        // Array: joined with blank lines.
        assert_eq!(hover_contents_to_markdown(&json!(["a", "b"])), "a\n\nb");
    }

    #[test]
    fn convert_hover_empty_is_none() {
        let index = LineIndex::new("fn main() {}");
        assert_eq!(convert_hover(&index, &Value::Null), (None, None));
        let blank = json!({"contents": {"kind": "markdown", "value": "   "}});
        assert_eq!(convert_hover(&index, &blank), (None, None));
    }

    // ── Provider navigate/hover over a fake LSP server ──────────────────────

    use super::super::lsp_codec::{LspDecoder, encode};
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A minimal fake LSP server: for every client request it records the
    /// `(method, params)` and replies with the configured result for that
    /// method (or `null`). Notifications are ignored. Enough to drive the
    /// provider's definition/hover round-trip without a real rust-analyzer.
    async fn fake_lsp<R, W>(
        mut reader: R,
        mut writer: W,
        responses: HashMap<String, Value>,
        captured: std::sync::Arc<StdMutex<Vec<(String, Value)>>>,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut decoder = LspDecoder::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            decoder.extend(&chunk[..n]);
            while let Ok(Some(msg)) = decoder.next() {
                let Some(id) = msg.get("id").cloned() else {
                    continue; // notification
                };
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                captured
                    .lock()
                    .expect("captured mutex poisoned")
                    .push((method.clone(), params));
                let result = responses.get(&method).cloned().unwrap_or(Value::Null);
                let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
                let framed = encode(&response);
                let _ = writer.write_all(&framed).await;
                let _ = writer.flush().await;
            }
        }
    }

    /// Captured `(method, params)` requests the fake LSP server received.
    type CapturedRequests = std::sync::Arc<StdMutex<Vec<(String, Value)>>>;

    /// A `Ready` `RaActor` wired to a fake LSP, plus the captured-request log,
    /// the project output stream, and its receiver.
    type FakeLspHarness = (
        RaActor,
        CapturedRequests,
        Stream,
        mpsc::UnboundedReceiver<protocol::Envelope>,
    );

    /// Build an `RaActor` in the `Ready` phase with a single subscribed +
    /// opened file backed by a fake LSP server answering `responses`. Returns
    /// the actor, the captured-request log, and the output stream receiver.
    fn ready_actor_with_fake_lsp(
        root: &ProjectRootPath,
        path: &ProjectPath,
        text: &str,
        responses: HashMap<String, Value>,
    ) -> FakeLspHarness {
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        let captured = std::sync::Arc::new(StdMutex::new(Vec::new()));
        tokio::spawn(fake_lsp(c2s_r, s2c_w, responses, captured.clone()));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);

        let mut actor = test_actor(root.clone(), CodeIntelResourceMode::Full);
        actor.phase = Phase::Ready;
        actor.client = Some(client);

        let (tx, rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        actor.files.insert(
            path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: Arc::new(AtomicU64::new(1)),
                output: stream.clone(),
                text: text.to_owned(),
                absolute: absolute_path(path),
                model_version: None,
            },
        );
        actor.opened.insert(path.clone());
        (actor, captured, stream, rx)
    }

    #[tokio::test]
    async fn shutdown_command_terminates_actor_and_lsp_client() {
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        let captured = std::sync::Arc::new(StdMutex::new(Vec::new()));
        tokio::spawn(fake_lsp(c2s_r, s2c_w, HashMap::new(), captured.clone()));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);

        let root = ProjectRootPath("/repo".to_owned());
        let (tx, rx) = mpsc::unbounded_channel();
        let mut actor = RaActor::new(
            test_config(),
            root,
            CodeIntelResourceMode::Full,
            tx.clone(),
            None,
        );
        actor.client = Some(client);
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            actor.run(rx).await;
            let _ = done_tx.send(());
        });

        tx.send(RaCommand::Shutdown).expect("send shutdown");
        tokio::time::timeout(Duration::from_secs(2), done_rx)
            .await
            .expect("actor exits after shutdown")
            .expect("done sender should report exit");

        let methods = captured
            .lock()
            .expect("captured mutex poisoned")
            .iter()
            .map(|(method, _)| method.clone())
            .collect::<Vec<_>>();
        assert!(
            methods.iter().any(|method| method == "shutdown"),
            "shutdown command must drive LSP shutdown request, got {methods:?}"
        );
    }

    #[tokio::test]
    async fn resubscribe_same_version_new_stream_replays_status_and_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let text = "fn main() {}\n";
        std::fs::write(dir.path().join("main.rs"), text).expect("write main.rs");
        let mut responses = HashMap::new();
        responses.insert(
            "textDocument/semanticTokens/full".to_owned(),
            json!({"data": []}),
        );
        let (mut actor, _captured, _old_stream, mut old_rx) =
            ready_actor_with_fake_lsp(&root, &path, text, responses);

        actor.files.get_mut(&path).expect("file").model_version = Some(ProjectFileVersion(1));
        actor.last_file_statuses.insert(
            path.clone(),
            CodeIntelStatusPayload {
                scope: CodeIntelStatusScope::File {
                    path: path.clone(),
                    version: ProjectFileVersion(1),
                },
                state: CodeIntelState::Ready,
                resource_mode: CodeIntelResourceMode::Full,
                work_done: None,
                total_work: None,
                message: None,
            },
        );

        let (new_tx, mut new_rx) = mpsc::unbounded_channel();
        let new_stream = Stream::new(StreamPath("/project/p".to_owned()), new_tx);
        actor
            .handle_command(RaCommand::Subscribe {
                path: path.clone(),
                version: ProjectFileVersion(1),
                output: new_stream,
            })
            .await;

        let status: CodeIntelStatusPayload =
            recv_frame(&mut new_rx, FrameKind::CodeIntelStatus).await;
        assert_eq!(status.state, CodeIntelState::Ready);
        assert_eq!(
            status.scope,
            CodeIntelStatusScope::File {
                path: path.clone(),
                version: ProjectFileVersion(1)
            }
        );

        let model: CodeIntelFileModelPayload =
            recv_frame(&mut new_rx, FrameKind::CodeIntelFileModel).await;
        assert_eq!(model.path, path);
        assert_eq!(model.version, ProjectFileVersion(1));
        assert!(
            old_rx.try_recv().is_err(),
            "replay should target the new subscriber stream"
        );
    }

    /// Drain the receiver for the first frame of `kind`, deserializing it.
    async fn recv_frame<T: serde::de::DeserializeOwned>(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
        kind: FrameKind,
    ) -> T {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no {kind} frame within timeout");
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(envelope)) if envelope.kind == kind => {
                    return serde_json::from_value(envelope.payload).expect("payload deserializes");
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("output stream closed before {kind} arrived"),
                Err(_) => panic!("timed out waiting for {kind}"),
            }
        }
    }

    #[tokio::test]
    async fn navigate_resolves_definition_to_byte_range_cross_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());

        // Source: the call site `名前` starts at byte 12 ("fn main() { " is 12
        // ASCII bytes), the offset the client clicks.
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() { 名前(); }";
        std::fs::write(dir.path().join("main.rs"), main_text).expect("write main.rs");

        // Target: `helper` lives on line 1 after a multibyte line 0, so its byte
        // range tests both the multi-line and multibyte conversion of the
        // returned location. Line 0 "// 日本" is 9 bytes + '\n' = line 1 @ byte
        // 10; "pub fn " is 7 bytes → `helper` at line-bytes 7..13 → file 17..23.
        let lib_text = "// 日本\npub fn helper() {}\n";
        std::fs::write(dir.path().join("lib.rs"), lib_text).expect("write lib.rs");
        let lib_uri = file_uri(&dir.path().join("lib.rs")).unwrap();

        let definition = json!({
            "uri": lib_uri,
            "range": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 13}},
        });
        let mut responses = HashMap::new();
        responses.insert("textDocument/definition".to_owned(), definition);

        let (actor, captured, _stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &main_path, main_text, responses);

        actor.on_navigate(
            CodeIntelNavigatePayload {
                navigate_id: 7,
                path: main_path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
            },
            actor.files.get(&main_path).unwrap().output.clone(),
        );

        let result: CodeIntelNavigateResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelNavigateResult).await;
        assert_eq!(result.navigate_id, 7);
        assert_eq!(result.targets.len(), 1);
        let target = &result.targets[0];
        assert_eq!(target.path.relative_path, "lib.rs");
        assert_eq!(target.range, ByteRange { start: 17, end: 23 });

        // The request side converted byte 12 → UTF-16 position (0, 12).
        let requests = captured.lock().unwrap();
        let (_, params) = requests
            .iter()
            .find(|(method, _)| method == "textDocument/definition")
            .expect("definition request sent");
        assert_eq!(params["position"]["line"], json!(0));
        assert_eq!(params["position"]["character"], json!(12));
    }

    #[tokio::test]
    async fn navigate_returns_multiple_targets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() { thing(); }";
        std::fs::write(dir.path().join("main.rs"), main_text).expect("write main.rs");

        let a_text = "pub trait T { fn thing(&self); }\n";
        let b_text = "impl T for U { fn thing(&self) {} }\n";
        std::fs::write(dir.path().join("a.rs"), a_text).expect("write a.rs");
        std::fs::write(dir.path().join("b.rs"), b_text).expect("write b.rs");
        let a_uri = file_uri(&dir.path().join("a.rs")).unwrap();
        let b_uri = file_uri(&dir.path().join("b.rs")).unwrap();

        let definition = json!([
            {"uri": a_uri, "range": {"start": {"line": 0, "character": 17}, "end": {"line": 0, "character": 22}}},
            {"uri": b_uri, "range": {"start": {"line": 0, "character": 20}, "end": {"line": 0, "character": 25}}},
        ]);
        let mut responses = HashMap::new();
        responses.insert("textDocument/definition".to_owned(), definition);

        let (actor, _captured, stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &main_path, main_text, responses);

        actor.on_navigate(
            CodeIntelNavigatePayload {
                navigate_id: 1,
                path: main_path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
            },
            stream,
        );

        let result: CodeIntelNavigateResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelNavigateResult).await;
        assert_eq!(
            result.targets.len(),
            2,
            "both definitions surface (M2 picks first client-side)"
        );
        let rels: Vec<&str> = result
            .targets
            .iter()
            .map(|t| t.path.relative_path.as_str())
            .collect();
        assert!(rels.contains(&"a.rs") && rels.contains(&"b.rs"));
    }

    #[tokio::test]
    async fn hover_returns_markdown_and_byte_range() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() { 名前(); }";
        std::fs::write(dir.path().join("main.rs"), main_text).expect("write main.rs");

        // Hover range covers `名前`: UTF-16 chars 12..14 → file bytes 12..18.
        let hover = json!({
            "contents": {"kind": "markdown", "value": "```rust\nfn 名前()\n```"},
            "range": {"start": {"line": 0, "character": 12}, "end": {"line": 0, "character": 14}},
        });
        let mut responses = HashMap::new();
        responses.insert("textDocument/hover".to_owned(), hover);

        let (actor, _captured, stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &main_path, main_text, responses);

        actor.on_hover(
            CodeIntelHoverPayload {
                hover_id: 42,
                path: main_path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
            },
            stream,
        );

        let result: CodeIntelHoverResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelHoverResult).await;
        assert_eq!(result.hover_id, 42);
        let contents = result.contents.expect("hover has markdown");
        assert!(contents.contains("名前"), "hover markdown: {contents:?}");
        assert_eq!(result.range, Some(ByteRange { start: 12, end: 18 }));
    }

    #[tokio::test]
    async fn stale_on_demand_requests_emit_error_and_skip_lsp() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let text = "fn main() { foo(); }";
        std::fs::write(dir.path().join("main.rs"), text).expect("write main.rs");

        let (mut actor, captured, stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &path, text, HashMap::new());
        actor.files.get_mut(&path).unwrap().version = ProjectFileVersion(2);

        actor.on_navigate(
            CodeIntelNavigatePayload {
                navigate_id: 10,
                path: path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
            },
            stream.clone(),
        );
        let error: CodeIntelErrorPayload = recv_frame(&mut rx, FrameKind::CodeIntelError).await;
        assert_eq!(error.code, CodeIntelErrorCode::StaleVersion);
        assert!(matches!(
            error.context,
            CodeIntelErrorContext::Navigate {
                navigate_id: 10,
                ..
            }
        ));
        let result: CodeIntelNavigateResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelNavigateResult).await;
        assert!(result.targets.is_empty());

        actor.on_hover(
            CodeIntelHoverPayload {
                hover_id: 11,
                path: path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
            },
            stream.clone(),
        );
        let error: CodeIntelErrorPayload = recv_frame(&mut rx, FrameKind::CodeIntelError).await;
        assert_eq!(error.code, CodeIntelErrorCode::StaleVersion);
        assert!(matches!(
            error.context,
            CodeIntelErrorContext::Hover { hover_id: 11, .. }
        ));
        let result: CodeIntelHoverResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelHoverResult).await;
        assert!(result.contents.is_none());

        actor.on_find_references(
            CodeIntelFindReferencesPayload {
                references_id: 12,
                path: path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
                include_declaration: true,
            },
            stream,
        );
        let error: CodeIntelErrorPayload = recv_frame(&mut rx, FrameKind::CodeIntelError).await;
        assert_eq!(error.code, CodeIntelErrorCode::StaleVersion);
        assert!(matches!(
            error.context,
            CodeIntelErrorContext::FindReferences {
                references_id: 12,
                ..
            }
        ));
        let complete: CodeIntelReferencesCompletePayload =
            recv_frame(&mut rx, FrameKind::CodeIntelReferencesComplete).await;
        assert_eq!(complete.references_id, 12);
        assert!(complete.error.is_some());

        assert!(
            captured.lock().unwrap().is_empty(),
            "stale-version requests must not be sent to the LSP against current text"
        );
    }

    #[tokio::test]
    async fn navigate_drops_inflight_result_after_source_version_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let text = "fn main() { foo(); }";
        std::fs::write(dir.path().join("main.rs"), text).expect("write main.rs");

        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        tokio::spawn(delayed_definition_fake(
            c2s_r,
            s2c_w,
            json!({ "data": [] }),
            Duration::from_millis(20),
        ));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);

        let mut actor = test_actor(root.clone(), CodeIntelResourceMode::Full);
        actor.phase = Phase::Ready;
        actor.client = Some(client);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let version_cell = Arc::new(AtomicU64::new(1));
        actor.files.insert(
            path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: version_cell.clone(),
                output: stream.clone(),
                text: text.to_owned(),
                absolute: absolute_path(&path),
                model_version: None,
            },
        );
        actor.opened.insert(path.clone());

        actor.on_navigate(
            CodeIntelNavigatePayload {
                navigate_id: 33,
                path: path.clone(),
                version: ProjectFileVersion(1),
                offset: 12,
            },
            stream,
        );
        actor.files.get_mut(&path).unwrap().version = ProjectFileVersion(2);
        version_cell.store(2, Ordering::SeqCst);

        let error: CodeIntelErrorPayload = recv_frame(&mut rx, FrameKind::CodeIntelError).await;
        assert_eq!(error.code, CodeIntelErrorCode::StaleVersion);
        assert!(matches!(
            error.context,
            CodeIntelErrorContext::Navigate {
                navigate_id: 33,
                ..
            }
        ));
        let result: CodeIntelNavigateResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelNavigateResult).await;
        assert_eq!(result.navigate_id, 33);
        assert!(
            result.targets.is_empty(),
            "a definition computed against stale source text must not be emitted"
        );
    }

    #[tokio::test]
    async fn navigate_during_indexing_is_honest_empty() {
        // Not Ready ⇒ no request issued, an honest empty `targets` answer.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() {}";
        std::fs::write(dir.path().join("main.rs"), main_text).expect("write main.rs");

        let (mut actor, captured, stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &main_path, main_text, HashMap::new());
        actor.phase = Phase::Indexing;

        actor.on_navigate(
            CodeIntelNavigatePayload {
                navigate_id: 3,
                path: main_path,
                version: ProjectFileVersion(1),
                offset: 3,
            },
            stream,
        );

        let result: CodeIntelNavigateResultPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelNavigateResult).await;
        assert!(result.targets.is_empty());
        assert!(
            captured.lock().unwrap().is_empty(),
            "no LSP request should be issued while indexing"
        );
    }

    // ── M7: provider crash / bounded restart ───────────────────────────────

    /// Latest `CodeIntelStatus` and `CodeIntelError` frames currently queued.
    fn drain_status_and_error(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
    ) -> (
        Option<CodeIntelStatusPayload>,
        Option<CodeIntelErrorPayload>,
    ) {
        let mut status = None;
        let mut error = None;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::CodeIntelStatus => {
                    status =
                        Some(serde_json::from_value(envelope.payload).expect("status payload"));
                }
                FrameKind::CodeIntelError => {
                    error = Some(serde_json::from_value(envelope.payload).expect("error payload"));
                }
                _ => {}
            }
        }
        (status, error)
    }

    #[tokio::test]
    async fn client_exit_surfaces_failed_and_schedules_bounded_restart() {
        // §M7: an unexpected child exit while a file is subscribed must surface a
        // `Failed` status + a `ProviderCrashed` error (never a silent empty
        // model), clear in-flight/open state so the restart re-`didOpen`s, and
        // schedule a bounded restart — recoverable until the budget is spent,
        // then fatal.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("write main.rs");

        let (mut actor, _captured, _stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &path, "fn main() {}", HashMap::new());

        // First crash → Failed + a *non-fatal* ProviderCrashed, restart pending.
        actor.on_client_closed(LspServerExit {
            exit_status: Some("exit status: 1".to_owned()),
            stderr: Some("error: Unknown binary 'rust-analyzer'".to_owned()),
        });
        assert_eq!(actor.phase, Phase::Failed);
        assert_eq!(actor.restart_attempts, 1);
        assert!(
            actor.opened.is_empty(),
            "a crash clears `opened` so the restart re-didOpens the files"
        );
        assert!(actor.resolutions.is_empty());

        let (status, error) = drain_status_and_error(&mut rx);
        let status = status.expect("status frame");
        assert_eq!(status.state, CodeIntelState::Failed);
        assert_eq!(
            status.message.as_deref(),
            Some("language server exited unexpectedly")
        );
        let error = error.expect("a ProviderCrashed error frame");
        assert_eq!(error.code, CodeIntelErrorCode::ProviderCrashed);
        assert_eq!(error.message, "language server exited unexpectedly");
        assert_eq!(error.exit_status.as_deref(), Some("exit status: 1"));
        assert_eq!(
            error.stderr.as_deref(),
            Some("error: Unknown binary 'rust-analyzer'")
        );
        assert!(!error.fatal, "recoverable while a restart is pending");
        assert!(matches!(
            error.context,
            CodeIntelErrorContext::Provider { .. }
        ));

        // Bounded: repeated crashes exhaust the budget; the budget caps at MAX.
        actor.on_client_closed(LspServerExit::default()); // attempt 2
        actor.on_client_closed(LspServerExit::default()); // attempt 3 (== MAX)
        assert_eq!(actor.restart_attempts, MAX_RESTART_ATTEMPTS);
        let _ = drain_status_and_error(&mut rx);

        actor.on_client_closed(LspServerExit::default()); // budget spent → fatal, no further attempt
        let (_s, error) = drain_status_and_error(&mut rx);
        assert!(
            error.expect("fatal crash error").fatal,
            "fatal once the restart budget is exhausted"
        );
        assert_eq!(
            actor.restart_attempts, MAX_RESTART_ATTEMPTS,
            "restart attempts are capped — no crash-loop"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_subprocess_crash_emits_observable_restart_fatality() {
        let Some(python) = crate::process_env::find_executable_in_path("python3") else {
            eprintln!(
                "SKIP provider_subprocess_crash_emits_observable_restart_fatality: `python3` not found on PATH"
            );
            return;
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let counter = dir.path().join("invocations");
        let fake_lsp = dir.path().join("fake-lsp.py");
        let script = r#"#!/usr/bin/env python3
import json
import os
import sys
import time

COUNTER = r'''__COUNTER__'''

def invocation():
    try:
        with open(COUNTER, "r", encoding="utf-8") as fh:
            count = int(fh.read().strip() or "0")
    except FileNotFoundError:
        count = 0
    with open(COUNTER, "w", encoding="utf-8") as fh:
        fh.write(str(count + 1))
    return count

def read_message():
    header = b""
    while b"\r\n\r\n" not in header:
        byte = sys.stdin.buffer.read(1)
        if not byte:
            return None
        header += byte
    raw_header, rest = header.split(b"\r\n\r\n", 1)
    length = None
    for line in raw_header.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
            break
    if length is None:
        return None
    body = rest + sys.stdin.buffer.read(length - len(rest))
    return json.loads(body.decode("utf-8"))

def write_message(message):
    body = json.dumps(message, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

if invocation() > 0:
    sys.stderr.write("fake restart startup crash")
    sys.stderr.flush()
    sys.exit(7)

while True:
    message = read_message()
    if message is None:
        sys.exit(0)
    method = message.get("method")
    if method == "initialize":
        write_message({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {"capabilities": {}},
        })
        write_message({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"token": "init", "value": {"kind": "end"}},
        })
    elif method == "textDocument/didOpen":
        uri = message["params"]["textDocument"]["uri"]
        write_message({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": []},
        })
        sys.stderr.write("fake lsp runtime crash")
        sys.stderr.flush()
        time.sleep(0.01)
        sys.exit(7)
"#
        .replace("#!/usr/bin/env python3", &format!("#!{}", python.display()))
        .replace("__COUNTER__", &counter.to_string_lossy());
        write_executable(&fake_lsp, &script);

        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"ci_crash\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(dir.path().join("src")).expect("create src");
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("write main.rs");

        *TEST_CRASHING_LSP
            .lock()
            .expect("test crashing LSP mutex poisoned") = Some(fake_lsp);

        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "src/main.rs".to_owned(),
        };
        let mut provider = LspProvider::new(
            crashing_test_config(),
            root.clone(),
            CodeIntelResourceMode::Full,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        provider.subscribe(path, ProjectFileVersion(1), stream);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_failed = false;
        let mut nonfatal_errors = 0;
        let fatal_error = loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "provider did not emit fatal crash after bounded restart attempts"
            );
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let envelope = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("frame before deadline")
                .expect("stream open");
            match envelope.kind {
                FrameKind::CodeIntelStatus => {
                    let status: CodeIntelStatusPayload =
                        serde_json::from_value(envelope.payload).expect("status payload");
                    saw_failed |= status.state == CodeIntelState::Failed;
                }
                FrameKind::CodeIntelError => {
                    let error: CodeIntelErrorPayload =
                        serde_json::from_value(envelope.payload).expect("error payload");
                    if !matches!(error.context, CodeIntelErrorContext::Provider { .. })
                        || !matches!(
                            error.code,
                            CodeIntelErrorCode::ProviderCrashed | CodeIntelErrorCode::Timeout
                        )
                    {
                        continue;
                    }
                    assert!(
                        !error.message.contains("fake lsp runtime crash")
                            && !error.message.contains("fake restart startup crash"),
                        "stderr must stay in the typed stderr field, not in message: {error:?}"
                    );
                    if error.fatal {
                        break error;
                    }
                    nonfatal_errors += 1;
                }
                _ => {}
            }
        };

        assert!(saw_failed, "crash path should emit Failed status");
        assert_eq!(
            nonfatal_errors, MAX_RESTART_ATTEMPTS as usize,
            "restart failures should be recoverable until the budget is exhausted; fatal={fatal_error:?}"
        );
        assert!(
            fatal_error
                .stderr
                .as_deref()
                .is_some_and(|stderr| stderr.contains("fake restart startup crash")),
            "fatal error should retain bounded stderr details: {fatal_error:?}"
        );
        assert!(
            fatal_error.exit_status.is_some(),
            "fatal error should carry the subprocess exit status: {fatal_error:?}"
        );

        drop(provider);
        *TEST_CRASHING_LSP
            .lock()
            .expect("test crashing LSP mutex poisoned") = None;
    }

    #[tokio::test]
    async fn restart_redidopens_still_subscribed_files() {
        // The re-`didOpen` step the restart runs after a successful re-spawn:
        // every still-subscribed file is re-opened against the fresh language
        // server (re-using its stored output + version), so the new process sees
        // the same documents. Driven here against a recording fake LSP.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("write main.rs");

        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        let captured = std::sync::Arc::new(StdMutex::new(Vec::new()));
        tokio::spawn(recording_lsp(
            c2s_r,
            s2c_w,
            HashMap::new(),
            captured.clone(),
        ));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);

        let mut actor = test_actor(root.clone(), CodeIntelResourceMode::Full);
        actor.phase = Phase::Ready;
        actor.client = Some(client);
        let (tx, _rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        actor.files.insert(
            path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: Arc::new(AtomicU64::new(1)),
                output: stream,
                text: String::new(),
                absolute: absolute_path(&path),
                model_version: None,
            },
        );
        // Post-crash, `opened` is empty; re-open every subscribed file.
        let paths: Vec<ProjectPath> = actor.files.keys().cloned().collect();
        for p in paths {
            actor.open_file(p).await;
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_didopen = false;
        while tokio::time::Instant::now() < deadline && !saw_didopen {
            saw_didopen = captured.lock().unwrap().iter().any(|(method, params)| {
                method == "textDocument/didOpen"
                    && params["textDocument"]["languageId"] == json!("rust")
            });
            if !saw_didopen {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(
            saw_didopen,
            "the restart re-didOpens every still-subscribed file"
        );
        assert!(actor.opened.contains(&path));
    }

    // ── M3: semanticTokens decode (pure) ───────────────────────────────────

    fn legend(types: &[&str], modifiers: &[&str]) -> SemanticLegend {
        SemanticLegend {
            token_types: types.iter().map(|s| s.to_string()).collect(),
            token_modifiers: modifiers.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn decode_semantic_tokens_to_byte_ranges_multibyte_multiline() {
        // type 0=variable, 1=function, 2=keyword (filtered out). modifier bit
        // 0=declaration (→ Definition role), bit 1=definition.
        let legend = legend(
            &["variable", "function", "keyword"],
            &["declaration", "definition"],
        );
        let text = "let x = foo\nbar 名";
        let index = LineIndex::new(text);
        // Groups of 5: deltaLine, deltaStartChar, length, tokenType, modifiers.
        let data = json!([
            0, 0, 3, 2, 0, // "let" keyword  -> filtered
            0, 4, 1, 0, 1, // "x" variable, declaration modifier -> Definition
            0, 4, 3, 1, 0, // "foo" function -> Reference
            1, 4, 1, 0, 0, // "名" variable (line 1, col 4) -> Reference
        ]);
        let occ = decode_semantic_occurrences(&data, &legend, &index, text);
        assert_eq!(occ.len(), 3, "keyword token filtered out");

        assert_eq!(occ[0].range, ByteRange { start: 4, end: 5 });
        assert_eq!(occ[0].role, CodeIntelRole::Definition);
        assert_eq!(occ[0].display, "x");
        assert!(occ[0].definition.is_empty());

        assert_eq!(occ[1].range, ByteRange { start: 8, end: 11 });
        assert_eq!(occ[1].role, CodeIntelRole::Reference);
        assert_eq!(occ[1].display, "foo");

        // "名" is 3 UTF-8 bytes on line 1 (which starts at byte 12, after the
        // "let x = foo\n" prefix); "bar " is 4 bytes → 16..19.
        assert_eq!(occ[2].range, ByteRange { start: 16, end: 19 });
        assert_eq!(occ[2].display, "名");
    }

    #[test]
    fn decode_semantic_tokens_empty_or_unknown_legend_yields_nothing() {
        let index = LineIndex::new("fn main() {}");
        // No legend → every type index is out of range → not navigable.
        let occ = decode_semantic_occurrences(
            &json!([0, 0, 2, 0, 0]),
            &SemanticLegend::default(),
            &index,
            "fn main() {}",
        );
        assert!(occ.is_empty());
        // Non-array data is an honest empty, not a panic.
        assert!(
            decode_semantic_occurrences(&Value::Null, &legend(&["variable"], &[]), &index, "x")
                .is_empty()
        );
    }

    #[test]
    fn decode_semantic_tokens_saturates_untrusted_end_character() {
        let text = "abcdef";
        let index = LineIndex::new(text);
        let occ = decode_semantic_occurrences(
            &json!([0, u32::MAX - 1, 10u32, 0, 0]),
            &legend(&["variable"], &[]),
            &index,
            text,
        );
        assert_eq!(occ.len(), 1);
        assert_eq!(
            occ[0].range,
            ByteRange { start: 6, end: 6 },
            "huge untrusted semantic-token columns clamp at line end without overflowing"
        );
    }

    #[test]
    fn reprioritize_moves_visible_occurrences_to_front() {
        let occ = |s: u32, e: u32| CodeIntelOccurrence {
            range: ByteRange { start: s, end: e },
            role: CodeIntelRole::Reference,
            display: String::new(),
            definition: Vec::new(),
        };
        let occurrences = vec![occ(0, 5), occ(10, 15), occ(20, 25), occ(30, 35)];
        let mut queue: VecDeque<usize> = (0..4).collect();
        // Visible window [12, 32) intersects occurrences 1, 2, 3 (not 0).
        reprioritize(&mut queue, &occurrences, ByteRange { start: 12, end: 32 });
        assert_eq!(
            queue.into_iter().collect::<Vec<_>>(),
            vec![1, 2, 3, 0],
            "visible-intersecting indices move to the front, stable order"
        );
    }

    // ── M3: incremental push driver over a fake LSP ────────────────────────

    /// Spawn a fake LSP answering `responses` and return a connected
    /// [`LspRequester`] plus the captured-request log. The client is leaked so
    /// the connection survives for the duration of the test.
    fn fake_requester(responses: HashMap<String, Value>) -> (LspRequester, CapturedRequests) {
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        let captured = std::sync::Arc::new(StdMutex::new(Vec::new()));
        tokio::spawn(fake_lsp(c2s_r, s2c_w, responses, captured.clone()));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        // Keep the client alive for the whole test (its drop aborts the actor).
        std::mem::forget(client);
        (requester, captured)
    }

    /// Collect every `CodeIntelFileModel` frame currently queued on `rx`.
    fn drain_models(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
    ) -> Vec<CodeIntelFileModelPayload> {
        let mut out = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            if envelope.kind == FrameKind::CodeIntelFileModel {
                out.push(serde_json::from_value(envelope.payload).expect("file model payload"));
            }
        }
        out
    }

    #[tokio::test]
    async fn semantic_tokens_failure_surfaces_error_status_and_model_failed() {
        let (c2s_w, _c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        drop(s2c_w);
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        std::mem::forget(client);

        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let uri = file_uri(&dir.path().join("main.rs")).unwrap();
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), output_tx);
        let (_visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

        let job = ModelJob {
            requester,
            root,
            path: path.clone(),
            version: ProjectFileVersion(1),
            uri,
            text: "fn main() {}".to_owned(),
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Full,
            tuning: ModelTuning::DEFAULT,
            visible_rx,
            cancel_rx,
            model_failed_tx: Some(cmd_tx),
        };
        job.run().await;

        let mut saw_error = false;
        let mut saw_failed_status = false;
        let mut saw_model = false;
        while let Ok(envelope) = output_rx.try_recv() {
            match envelope.kind {
                FrameKind::CodeIntelError => {
                    let error: CodeIntelErrorPayload =
                        serde_json::from_value(envelope.payload).unwrap();
                    saw_error = error.message.contains("semanticTokens/full failed")
                        && matches!(error.context, CodeIntelErrorContext::Subscribe { .. });
                }
                FrameKind::CodeIntelStatus => {
                    let status: CodeIntelStatusPayload =
                        serde_json::from_value(envelope.payload).unwrap();
                    saw_failed_status = status.state == CodeIntelState::Failed
                        && status
                            .message
                            .as_deref()
                            .is_some_and(|message| message.contains("semanticTokens/full failed"));
                }
                FrameKind::CodeIntelFileModel => saw_model = true,
                _ => {}
            }
        }
        assert!(saw_error, "semantic-token failure must emit CodeIntelError");
        assert!(
            saw_failed_status,
            "semantic-token failure must mark the file status Failed"
        );
        assert!(
            !saw_model,
            "failed semantic tokens must not emit a fake model"
        );
        match cmd_rx.try_recv().expect("model failure command sent") {
            RaCommand::ModelFailed {
                path: failed_path,
                version,
            } => {
                assert_eq!(failed_path, path);
                assert_eq!(version, ProjectFileVersion(1));
            }
            _ => panic!("expected ModelFailed command"),
        }
    }

    #[tokio::test]
    async fn model_failure_clears_pushed_marker_for_retry() {
        let root = ProjectRootPath("/repo".to_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let mut actor = test_actor(root, CodeIntelResourceMode::Full);
        let (output_tx, _output_rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), output_tx);
        actor.files.insert(
            path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: Arc::new(AtomicU64::new(1)),
                output,
                text: "fn main() {}".to_owned(),
                absolute: absolute_path(&path),
                model_version: Some(ProjectFileVersion(1)),
            },
        );
        let (visible_tx, _visible_rx) = mpsc::unbounded_channel();
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        actor.resolutions.insert(
            path.clone(),
            ResolutionHandle {
                version: ProjectFileVersion(1),
                visible_tx,
                _cancel_tx: cancel_tx,
            },
        );

        actor
            .handle_command(RaCommand::ModelFailed {
                path: path.clone(),
                version: ProjectFileVersion(1),
            })
            .await;

        assert_eq!(
            actor.files.get(&path).unwrap().model_version,
            None,
            "a semantic-token failure must not leave the version marked pushed"
        );
        assert!(
            !actor.resolutions.contains_key(&path),
            "failed model resolution handle is cleared so a later Ready can retry"
        );
    }

    #[tokio::test]
    async fn model_push_streams_partial_then_complete_with_byte_targets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());

        // Source: a single navigable occurrence `foo` at bytes 12..15.
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() { foo() }";
        let main_uri = file_uri(&dir.path().join("main.rs")).unwrap();

        // Target: `foo` at bytes 7..10 in lib.rs (read from disk to convert).
        let lib_text = "pub fn foo() {}\n";
        std::fs::write(dir.path().join("lib.rs"), lib_text).expect("write lib.rs");
        let lib_uri = file_uri(&dir.path().join("lib.rs")).unwrap();

        let mut responses = HashMap::new();
        responses.insert(
            "textDocument/semanticTokens/full".to_owned(),
            json!({ "data": [0, 12, 3, 0, 0] }), // one "function" token at col 12
        );
        responses.insert(
            "textDocument/definition".to_owned(),
            json!({
                "uri": lib_uri,
                "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 10}},
            }),
        );
        let (requester, captured) = fake_requester(responses);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let (_visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let job = ModelJob {
            requester,
            root: root.clone(),
            path: main_path.clone(),
            version: ProjectFileVersion(1),
            uri: main_uri,
            text: main_text.to_owned(),
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Full,
            tuning: ModelTuning::DEFAULT,
            visible_rx,
            cancel_rx,
            model_failed_tx: None,
        };
        job.run().await;

        let models = drain_models(&mut rx);
        assert!(
            models.len() >= 2,
            "expected a Partial then Complete: {models:?}"
        );

        // First frame: Partial, full occurrence set, definition not yet resolved.
        let first = &models[0];
        assert_eq!(first.completeness, CodeIntelCompleteness::Partial);
        assert_eq!(first.model_range, CodeIntelModelRange::FullFile);
        assert_eq!(first.version, ProjectFileVersion(1));
        assert_eq!(first.occurrences.len(), 1);
        assert_eq!(first.occurrences[0].range, ByteRange { start: 12, end: 15 });
        assert!(first.occurrences[0].definition.is_empty());

        // Final frame: Complete, the occurrence now carries the byte target.
        let last = models.last().unwrap();
        assert_eq!(last.completeness, CodeIntelCompleteness::Complete);
        let resolved = last
            .occurrences
            .iter()
            .find(|o| o.range == ByteRange { start: 12, end: 15 })
            .expect("resolved occurrence in a streamed frame");
        assert_eq!(resolved.definition.len(), 1);
        assert_eq!(resolved.definition[0].path.relative_path, "lib.rs");
        assert_eq!(
            resolved.definition[0].range,
            ByteRange { start: 7, end: 10 }
        );

        // The request side converted byte 12 → UTF-16 position (0, 12).
        let requests = captured.lock().unwrap();
        let (_, params) = requests
            .iter()
            .find(|(method, _)| method == "textDocument/definition")
            .expect("a definition request was issued");
        assert_eq!(params["position"]["line"], json!(0));
        assert_eq!(params["position"]["character"], json!(12));
    }

    #[tokio::test]
    async fn model_push_cancelled_before_decode_emits_nothing() {
        // A silent fake server: it holds the connection open but never answers,
        // so the job parks awaiting semanticTokens in its cancel-aware select.
        // Dropping the cancel sender returns it with zero frames emitted.
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            // Keep the write half alive (so the client never sees EOF and the
            // request stays pending) while draining whatever the client sends.
            let _writer = s2c_w;
            let mut reader = c2s_r;
            let mut buf = [0u8; 1024];
            while let Ok(n) = reader.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        std::mem::forget(client);

        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let uri = file_uri(&dir.path().join("main.rs")).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let (_visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let job = ModelJob {
            requester,
            root,
            path,
            version: ProjectFileVersion(1),
            uri,
            text: "fn main() {}".to_owned(),
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Full,
            tuning: ModelTuning::DEFAULT,
            visible_rx,
            cancel_rx,
            model_failed_tx: None,
        };
        let handle = tokio::spawn(job.run());
        // Let the job reach its parked select on semanticTokens, then cancel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(cancel_tx);
        handle.await.expect("driver task joins");

        let models = drain_models(&mut rx);
        assert!(
            models.is_empty(),
            "a push cancelled before decode emits no frames: {models:?}"
        );
    }

    #[tokio::test]
    async fn subscribe_at_new_version_restarts_model_push() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() { foo() }";
        std::fs::write(dir.path().join("main.rs"), main_text).expect("write main.rs");

        let mut responses = HashMap::new();
        responses.insert(
            "textDocument/semanticTokens/full".to_owned(),
            json!({ "data": [0, 12, 3, 0, 0] }),
        );
        responses.insert("textDocument/definition".to_owned(), Value::Null);

        let (actor, _captured, stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &main_path, main_text, responses);
        let mut actor = actor;
        actor.legend = legend(&["function"], &[]);

        // Subscribe v1 → kicks off the model push for version 1.
        actor
            .handle_command(RaCommand::Subscribe {
                path: main_path.clone(),
                version: ProjectFileVersion(1),
                output: stream.clone(),
            })
            .await;

        // Re-subscribe at v2 (contents changed) → supersedes v1 and restarts.
        actor
            .handle_command(RaCommand::Subscribe {
                path: main_path.clone(),
                version: ProjectFileVersion(2),
                output: stream.clone(),
            })
            .await;

        // Give the detached drivers time to push their initial Partial frames.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_v2 = false;
        while tokio::time::Instant::now() < deadline {
            for model in drain_models(&mut rx) {
                if model.version == ProjectFileVersion(2) {
                    saw_v2 = true;
                }
            }
            if saw_v2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saw_v2,
            "a re-subscribe at a new version must restart the push at that version"
        );
        // The handle tracked for the file is the v2 resolution (v1 superseded).
        assert_eq!(
            actor.resolutions.get(&main_path).map(|h| h.version),
            Some(ProjectFileVersion(2))
        );
    }

    #[tokio::test]
    async fn unsubscribe_cancels_resolution_handle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let main_text = "fn main() { foo() }";
        std::fs::write(dir.path().join("main.rs"), main_text).expect("write main.rs");

        let mut responses = HashMap::new();
        responses.insert(
            "textDocument/semanticTokens/full".to_owned(),
            json!({ "data": [0, 12, 3, 0, 0] }),
        );
        responses.insert("textDocument/definition".to_owned(), Value::Null);

        let (actor, _captured, stream, _rx) =
            ready_actor_with_fake_lsp(&root, &main_path, main_text, responses);
        let mut actor = actor;
        actor.legend = legend(&["function"], &[]);

        actor
            .handle_command(RaCommand::Subscribe {
                path: main_path.clone(),
                version: ProjectFileVersion(1),
                output: stream,
            })
            .await;
        assert!(
            actor.resolutions.contains_key(&main_path),
            "a resolution handle exists after subscribe"
        );

        actor
            .handle_command(RaCommand::Unsubscribe {
                path: main_path.clone(),
            })
            .await;
        assert!(
            !actor.resolutions.contains_key(&main_path),
            "unsubscribe drops the resolution handle (cancelling the driver)"
        );
        assert!(!actor.files.contains_key(&main_path));
    }

    /// A fake LSP that answers `semanticTokens/full` immediately but only replies
    /// to `textDocument/definition` after `delay`. The delay lets a test cancel
    /// the job while a definition request is still in flight, then assert that no
    /// frame is emitted once the (now-aborted) request would have resolved.
    async fn delayed_definition_fake<R, W>(
        mut reader: R,
        mut writer: W,
        tokens: Value,
        delay: Duration,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut decoder = LspDecoder::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            decoder.extend(&chunk[..n]);
            while let Ok(Some(msg)) = decoder.next() {
                let Some(id) = msg.get("id").cloned() else {
                    continue;
                };
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                match method.as_str() {
                    "textDocument/semanticTokens/full" => {
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": tokens.clone()});
                        let _ = writer.write_all(&encode(&resp)).await;
                        let _ = writer.flush().await;
                    }
                    "textDocument/definition" => {
                        // Slow reply: by the time it lands the test has cancelled
                        // and the owning JoinSet has aborted the task that awaited
                        // it, so the reply is delivered to a dropped receiver.
                        tokio::time::sleep(delay).await;
                        let resp = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                        let _ = writer.write_all(&encode(&resp)).await;
                        let _ = writer.flush().await;
                    }
                    _ => {}
                }
            }
        }
    }

    #[tokio::test]
    async fn cancellation_stops_definition_frames() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let uri = file_uri(&dir.path().join("main.rs")).unwrap();

        // One navigable occurrence; definition answered only after a long delay,
        // so cancellation reliably wins the race.
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        tokio::spawn(delayed_definition_fake(
            c2s_r,
            s2c_w,
            json!({ "data": [0, 12, 3, 0, 0] }),
            Duration::from_millis(500),
        ));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        std::mem::forget(client);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let (_visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let job = ModelJob {
            requester,
            root,
            path,
            version: ProjectFileVersion(1),
            uri,
            text: "fn main() { foo() }".to_owned(),
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Full,
            tuning: ModelTuning::DEFAULT,
            visible_rx,
            cancel_rx,
            model_failed_tx: None,
        };
        let handle = tokio::spawn(job.run());

        // The initial Partial arrives (semanticTokens done; a definition task is
        // now in flight against the delayed fake).
        let first: CodeIntelFileModelPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelFileModel).await;
        assert_eq!(first.completeness, CodeIntelCompleteness::Partial);
        assert_eq!(first.occurrences.len(), 1);

        // Cancel while the definition is still "computing". The driver returns
        // promptly (it does not block awaiting the in-flight task), and dropping
        // the job's owned `JoinSet` aborts that task.
        drop(cancel_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("driver returns promptly on cancel, not blocked on the task")
            .expect("driver task joins");

        // Wait past the fake's definition delay: a still-running task would have
        // resolved by now. No further frame (no Complete, no late batch) may be
        // emitted after cancellation.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let late = drain_models(&mut rx);
        assert!(
            late.is_empty(),
            "no definition frame may be emitted after cancellation: {late:?}"
        );
    }

    // ── M4: watcher-driven version change re-reads + re-pushes ──────────────

    /// A fake LSP that records **every** message it receives (notifications
    /// included — `fake_lsp` skips those), so a test can assert that a
    /// `textDocument/didChange` notification was sent. Requests are answered
    /// from `responses`; notifications are recorded and otherwise ignored.
    async fn recording_lsp<R, W>(
        mut reader: R,
        mut writer: W,
        responses: HashMap<String, Value>,
        captured: CapturedRequests,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut decoder = LspDecoder::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            decoder.extend(&chunk[..n]);
            while let Ok(Some(msg)) = decoder.next() {
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                captured
                    .lock()
                    .expect("captured mutex poisoned")
                    .push((method.clone(), params));
                // Only requests (those carrying an `id`) get a reply.
                if let Some(id) = msg.get("id").cloned() {
                    let result = responses.get(&method).cloned().unwrap_or(Value::Null);
                    let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    let _ = writer.write_all(&encode(&response)).await;
                    let _ = writer.flush().await;
                }
            }
        }
    }

    #[tokio::test]
    async fn watcher_version_change_reopens_and_repushes_at_new_version() {
        // §M4: a watched change (delivered as `FileVersionChanged`) must re-read
        // the file, send `textDocument/didChange`, and restart the whole-file
        // model push at the *new* version — superseding the old in-flight
        // resolution and re-using the file's stored output stream (no
        // re-subscribe). And it must be monotonic: a stale/equal version is a
        // no-op.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let main_path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let v1_text = "fn main() { foo() }";
        std::fs::write(dir.path().join("main.rs"), v1_text).expect("write v1");

        let mut responses = HashMap::new();
        responses.insert(
            "textDocument/semanticTokens/full".to_owned(),
            json!({ "data": [0, 12, 3, 0, 0] }),
        );
        responses.insert("textDocument/definition".to_owned(), Value::Null);

        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        let captured = std::sync::Arc::new(StdMutex::new(Vec::new()));
        tokio::spawn(recording_lsp(c2s_r, s2c_w, responses, captured.clone()));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);

        let mut actor = test_actor(root.clone(), CodeIntelResourceMode::Full);
        actor.phase = Phase::Ready;
        actor.client = Some(client);
        actor.legend = legend(&["function"], &[]);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/p".to_owned()), tx);
        actor.files.insert(
            main_path.clone(),
            SubscribedFile {
                version: ProjectFileVersion(1),
                version_cell: Arc::new(AtomicU64::new(1)),
                output: stream.clone(),
                text: v1_text.to_owned(),
                absolute: absolute_path(&main_path),
                model_version: None,
            },
        );
        actor.opened.insert(main_path.clone());

        // Kick off the v1 model push.
        actor
            .handle_command(RaCommand::Subscribe {
                path: main_path.clone(),
                version: ProjectFileVersion(1),
                output: stream.clone(),
            })
            .await;

        // External edit lands on disk, then the watcher-driven bump arrives.
        let v2_text = "fn main() { bar() }";
        std::fs::write(dir.path().join("main.rs"), v2_text).expect("write v2");
        actor
            .handle_command(RaCommand::FileVersionChanged {
                path: main_path.clone(),
                version: ProjectFileVersion(2),
            })
            .await;

        // The file was re-read and the tracked version advanced.
        assert_eq!(actor.files.get(&main_path).unwrap().text, v2_text);
        assert_eq!(
            actor.files.get(&main_path).unwrap().version,
            ProjectFileVersion(2)
        );
        // The resolution handle is now v2 — the v1 driver was superseded
        // (its handle dropped, which cancels it).
        assert_eq!(
            actor.resolutions.get(&main_path).map(|h| h.version),
            Some(ProjectFileVersion(2))
        );

        // A new-version model frame is pushed onto the stored output stream.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_v2 = false;
        while tokio::time::Instant::now() < deadline {
            for model in drain_models(&mut rx) {
                if model.version == ProjectFileVersion(2) {
                    saw_v2 = true;
                }
            }
            if saw_v2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saw_v2,
            "a watched change must re-push the model at the new version"
        );

        // A `didChange` was sent to rust-analyzer, carrying the new version.
        {
            let requests = captured.lock().unwrap();
            let (_, change_params) = requests
                .iter()
                .find(|(method, _)| method == "textDocument/didChange")
                .expect("a didChange notification was sent");
            assert_eq!(change_params["textDocument"]["version"], json!(2));
            assert_eq!(
                change_params["contentChanges"][0]["text"],
                json!(v2_text),
                "didChange carries the freshly re-read contents"
            );
        }

        // Monotonic: a stale (older/equal) version change is a no-op — no
        // version regression, no duplicate resolution restart.
        let handle_before = actor.resolutions.get(&main_path).map(|h| h.version);
        actor
            .handle_command(RaCommand::FileVersionChanged {
                path: main_path.clone(),
                version: ProjectFileVersion(1),
            })
            .await;
        assert_eq!(
            actor.files.get(&main_path).unwrap().version,
            ProjectFileVersion(2),
            "an older version change must not regress the tracked version"
        );
        assert_eq!(
            actor.resolutions.get(&main_path).map(|h| h.version),
            handle_before,
            "an older version change must not restart resolution"
        );
    }

    #[tokio::test]
    async fn version_change_for_unsubscribed_file_is_ignored() {
        // A version bump for a file this provider doesn't track is a no-op
        // (the service filters by subscription, but the provider is defensive).
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let mut actor = test_actor(root.clone(), CodeIntelResourceMode::Full);
        actor.phase = Phase::Ready;
        let path = ProjectPath {
            root,
            relative_path: "untracked.rs".to_owned(),
        };
        actor
            .handle_command(RaCommand::FileVersionChanged {
                path: path.clone(),
                version: ProjectFileVersion(9),
            })
            .await;
        assert!(!actor.files.contains_key(&path));
        assert!(!actor.resolutions.contains_key(&path));
    }

    // ── M5: find-references ─────────────────────────────────────────────────

    #[test]
    fn references_progress_splits_live_cancel_supersede() {
        assert_eq!(references_progress(5, 5), ReferencesProgress::Live);
        assert_eq!(references_progress(0, 5), ReferencesProgress::Cancelled);
        assert_eq!(references_progress(9, 5), ReferencesProgress::Superseded);
    }

    #[test]
    fn build_reference_lines_groups_by_line_with_byte_previews() {
        // Multibyte + multi-occurrence: two refs on line 1 (the CJK "名前"
        // identifier and a later one) exercise line-relative byte conversion.
        let text = "fn main() {\n    名前(); 名前;\n}\n";
        let index = LineIndex::new(text);
        // Line 1 "    名前(); 名前": "    " = 4 utf16 cols; "名前" = cols 4..6;
        // then "(); " (4 cols) → second "名前" at cols 10..12.
        let ranges = vec![(1, 4, 1, 6), (1, 10, 1, 12)];
        let (lines, count, truncated) = build_reference_lines(&index, &ranges);
        assert!(!truncated);
        assert_eq!(count, 2);
        assert_eq!(lines.len(), 1, "both refs collapse onto line 2");
        let line = &lines[0];
        assert_eq!(line.line_number, 2);
        assert_eq!(line.line_text, "    名前(); 名前;");
        // "    " is 4 bytes; first "名前" is bytes 4..10; "(); " is 4 bytes →
        // second "名前" at 14..20 (line-relative).
        assert_eq!(
            line.ranges,
            vec![
                ByteRange { start: 4, end: 10 },
                ByteRange { start: 14, end: 20 },
            ]
        );
    }

    #[test]
    fn build_reference_lines_marks_truncation_at_cap() {
        let text = "x\n";
        let index = LineIndex::new(text);
        let ranges: Vec<(u32, u32, u32, u32)> = (0..(MAX_REFERENCES_PER_FILE as u32 + 5))
            .map(|_| (0, 0, 0, 1))
            .collect();
        let (_lines, count, truncated) = build_reference_lines(&index, &ranges);
        assert!(truncated, "exceeding the per-file cap sets truncated");
        assert_eq!(count as usize, MAX_REFERENCES_PER_FILE);
    }

    /// Collect every references frame currently queued on `rx`.
    fn drain_references(
        rx: &mut mpsc::UnboundedReceiver<protocol::Envelope>,
    ) -> (
        Vec<CodeIntelReferencesResultsPayload>,
        Option<CodeIntelReferencesCompletePayload>,
    ) {
        let mut results = Vec::new();
        let mut complete = None;
        while let Ok(envelope) = rx.try_recv() {
            match envelope.kind {
                FrameKind::CodeIntelReferencesResults => {
                    results
                        .push(serde_json::from_value(envelope.payload).expect("results payload"));
                }
                FrameKind::CodeIntelReferencesComplete => {
                    complete =
                        Some(serde_json::from_value(envelope.payload).expect("complete payload"));
                }
                _ => {}
            }
        }
        (results, complete)
    }

    /// Build a references response across two in-root files plus one out-of-root
    /// file, write the in-root files to disk, and return (root, source uri/text,
    /// response, out-of-root tempdir kept alive).
    fn references_fixture() -> (
        ProjectRootPath,
        String,
        String,
        Value,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());

        let a_text = "fn foo() {}\nfoo();\n";
        std::fs::write(dir.path().join("a.rs"), a_text).expect("write a.rs");
        let a_uri = file_uri(&dir.path().join("a.rs")).unwrap();

        let b_text = "use crate::foo;\nfoo();\n";
        std::fs::write(dir.path().join("b.rs"), b_text).expect("write b.rs");
        let b_uri = file_uri(&dir.path().join("b.rs")).unwrap();

        // A file outside the project root must be filtered out of the results.
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("ext.rs"), "fn ext() {}\n").expect("write ext.rs");
        let out_uri = file_uri(&outside.path().join("ext.rs")).unwrap();

        let response = json!([
            {"uri": a_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}},
            {"uri": a_uri, "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}},
            {"uri": b_uri, "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}},
            {"uri": out_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}},
        ]);
        (root, a_uri, a_text.to_owned(), response, dir, outside)
    }

    fn references_job(
        root: ProjectRootPath,
        source_uri: String,
        source_text: String,
        references_id: u64,
        active: u64,
        responses: HashMap<String, Value>,
    ) -> (
        FindReferencesJob,
        CapturedRequests,
        mpsc::UnboundedReceiver<protocol::Envelope>,
    ) {
        let (requester, captured) = fake_requester(responses);
        let (tx, rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let job = FindReferencesJob {
            requester,
            root: root.clone(),
            references_id,
            path: ProjectPath {
                root,
                relative_path: "a.rs".to_owned(),
            },
            uri: source_uri,
            text: source_text,
            offset: 3,
            include_declaration: true,
            output,
            active: std::sync::Arc::new(AtomicU64::new(active)),
        };
        (job, captured, rx)
    }

    async fn hanging_references_lsp<R, W>(
        mut reader: R,
        writer: W,
        captured: std::sync::Arc<StdMutex<Vec<Value>>>,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let _writer = writer;
        let mut decoder = LspDecoder::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            decoder.extend(&chunk[..n]);
            while let Ok(Some(msg)) = decoder.next() {
                captured.lock().expect("captured mutex poisoned").push(msg);
            }
        }
    }

    fn hanging_references_requester() -> (LspRequester, std::sync::Arc<StdMutex<Vec<Value>>>) {
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        let captured = std::sync::Arc::new(StdMutex::new(Vec::new()));
        tokio::spawn(hanging_references_lsp(c2s_r, s2c_w, captured.clone()));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        std::mem::forget(client);
        (requester, captured)
    }

    async fn wait_for_lsp_method(
        captured: &std::sync::Arc<StdMutex<Vec<Value>>>,
        method: &str,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(message) = captured
                .lock()
                .expect("captured mutex poisoned")
                .iter()
                .find(|message| message.get("method").and_then(Value::as_str) == Some(method))
                .cloned()
            {
                return message;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for LSP method {method}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn find_references_streams_per_file_results_then_complete() {
        let (root, a_uri, a_text, response, _dir, _outside) = references_fixture();
        let mut responses = HashMap::new();
        responses.insert("textDocument/references".to_owned(), response);

        let (job, captured, mut rx) = references_job(root, a_uri, a_text, 1, 1, responses);
        job.run().await;

        let (results, complete) = drain_references(&mut rx);
        // Two in-root files stream; the out-of-root file is filtered out.
        assert_eq!(results.len(), 2, "one results frame per in-root file");
        for frame in &results {
            assert_eq!(frame.references_id, 1);
        }

        let a = results
            .iter()
            .find(|r| r.file.path.relative_path == "a.rs")
            .expect("a.rs streamed");
        assert_eq!(a.file.lines.len(), 2);
        assert_eq!(a.file.lines[0].line_number, 1);
        assert_eq!(a.file.lines[0].line_text, "fn foo() {}");
        assert_eq!(a.file.lines[0].ranges, vec![ByteRange { start: 3, end: 6 }]);
        assert_eq!(a.file.lines[1].line_number, 2);
        assert_eq!(a.file.lines[1].line_text, "foo();");
        assert_eq!(a.file.lines[1].ranges, vec![ByteRange { start: 0, end: 3 }]);

        let b = results
            .iter()
            .find(|r| r.file.path.relative_path == "b.rs")
            .expect("b.rs streamed");
        assert_eq!(b.file.lines.len(), 1);
        assert_eq!(b.file.lines[0].line_number, 2);
        assert_eq!(b.file.lines[0].ranges, vec![ByteRange { start: 0, end: 3 }]);

        let complete = complete.expect("terminal complete");
        assert_eq!(complete.references_id, 1);
        assert_eq!(complete.total_files, 2);
        assert_eq!(complete.total_references, 3, "out-of-root ref excluded");
        assert!(!complete.cancelled);
        assert!(complete.error.is_none());

        // The request carried the byte→UTF-16 position and includeDeclaration.
        let requests = captured.lock().unwrap();
        let (_, params) = requests
            .iter()
            .find(|(method, _)| method == "textDocument/references")
            .expect("references request sent");
        assert_eq!(params["position"]["line"], json!(0));
        assert_eq!(params["position"]["character"], json!(3));
        assert_eq!(params["context"]["includeDeclaration"], json!(true));
    }

    #[tokio::test]
    async fn find_references_superseded_drops_all_frames() {
        // The active id is a *different* (newer) query, so this driver must drop
        // every frame including the terminal — the newer query owns the stream.
        let (root, a_uri, a_text, response, _dir, _outside) = references_fixture();
        let mut responses = HashMap::new();
        responses.insert("textDocument/references".to_owned(), response);

        let (job, _captured, mut rx) = references_job(root, a_uri, a_text, 1, 2, responses);
        job.run().await;

        let (results, complete) = drain_references(&mut rx);
        assert!(results.is_empty(), "superseded query streams nothing");
        assert!(
            complete.is_none(),
            "superseded query emits no terminal frame"
        );
    }

    #[tokio::test]
    async fn find_references_cancel_midflight_sends_lsp_cancel_for_request_id() {
        let (root, a_uri, a_text, _response, _dir, _outside) = references_fixture();
        let (requester, captured) = hanging_references_requester();
        let active = std::sync::Arc::new(AtomicU64::new(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let job = FindReferencesJob {
            requester,
            root: root.clone(),
            references_id: 1,
            path: ProjectPath {
                root,
                relative_path: "a.rs".to_owned(),
            },
            uri: a_uri,
            text: a_text,
            offset: 3,
            include_declaration: true,
            output,
            active: active.clone(),
        };
        let handle = tokio::spawn(job.run());

        let request = wait_for_lsp_method(&captured, "textDocument/references").await;
        let request_id = request
            .get("id")
            .and_then(Value::as_i64)
            .expect("references request has numeric id");

        active.store(0, Ordering::SeqCst);
        let cancel = wait_for_lsp_method(&captured, "$/cancelRequest").await;
        assert_eq!(
            cancel
                .get("params")
                .and_then(|params| params.get("id"))
                .and_then(Value::as_i64),
            Some(request_id),
            "cancel notification must target the in-flight references request id"
        );

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cancelled job returns promptly")
            .expect("job task joins");
        let (results, complete) = drain_references(&mut rx);
        assert!(results.is_empty());
        let complete = complete.expect("cancelled completion emitted");
        assert_eq!(complete.references_id, 1);
        assert!(complete.cancelled);
    }

    #[tokio::test]
    async fn find_references_supersede_midflight_sends_lsp_cancel_for_request_id() {
        let (root, a_uri, a_text, _response, _dir, _outside) = references_fixture();
        let (requester, captured) = hanging_references_requester();
        let active = std::sync::Arc::new(AtomicU64::new(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let job = FindReferencesJob {
            requester,
            root: root.clone(),
            references_id: 1,
            path: ProjectPath {
                root,
                relative_path: "a.rs".to_owned(),
            },
            uri: a_uri,
            text: a_text,
            offset: 3,
            include_declaration: true,
            output,
            active: active.clone(),
        };
        let handle = tokio::spawn(job.run());

        let request = wait_for_lsp_method(&captured, "textDocument/references").await;
        let request_id = request
            .get("id")
            .and_then(Value::as_i64)
            .expect("references request has numeric id");

        active.store(2, Ordering::SeqCst);
        let cancel = wait_for_lsp_method(&captured, "$/cancelRequest").await;
        assert_eq!(
            cancel
                .get("params")
                .and_then(|params| params.get("id"))
                .and_then(Value::as_i64),
            Some(request_id),
            "supersede cancellation must target the in-flight references request id"
        );

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("superseded job returns promptly")
            .expect("job task joins");
        let (results, complete) = drain_references(&mut rx);
        assert!(results.is_empty());
        assert!(
            complete.is_none(),
            "superseded query still emits no terminal frame"
        );
    }

    #[tokio::test]
    async fn find_references_cancelled_emits_cancelled_complete() {
        // The active id was reset to 0 (explicit cancel): no per-file results,
        // but an honest terminal frame marked cancelled.
        let (root, a_uri, a_text, response, _dir, _outside) = references_fixture();
        let mut responses = HashMap::new();
        responses.insert("textDocument/references".to_owned(), response);

        let (job, _captured, mut rx) = references_job(root, a_uri, a_text, 1, 0, responses);
        job.run().await;

        let (results, complete) = drain_references(&mut rx);
        assert!(results.is_empty(), "cancelled query streams no results");
        let complete = complete.expect("cancelled terminal frame");
        assert_eq!(complete.references_id, 1);
        assert!(complete.cancelled);
        assert!(complete.error.is_none());
    }

    #[tokio::test]
    async fn find_references_request_failure_completes_with_error() {
        // No response configured for `textDocument/references`: the fake replies
        // `null`, which `parse_lsp_locations` reads as zero locations → an empty,
        // non-error completion. To exercise the *error* path we point the source
        // at a closed connection instead.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let uri = file_uri(&dir.path().join("main.rs")).unwrap();

        // A connection whose server half is immediately dropped → request errors.
        let (c2s_w, _c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        drop(s2c_w);
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        std::mem::forget(client);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let job = FindReferencesJob {
            requester,
            root: root.clone(),
            references_id: 7,
            path: ProjectPath {
                root,
                relative_path: "main.rs".to_owned(),
            },
            uri,
            text: "fn main() {}".to_owned(),
            offset: 3,
            include_declaration: true,
            output,
            active: std::sync::Arc::new(AtomicU64::new(7)),
        };
        job.run().await;

        let (results, complete) = drain_references(&mut rx);
        assert!(results.is_empty());
        let complete = complete.expect("terminal complete on error");
        assert_eq!(complete.references_id, 7);
        assert!(
            complete.error.is_some(),
            "request failure surfaces an error"
        );
        assert!(!complete.cancelled);
    }

    #[tokio::test]
    async fn provider_find_references_honest_empty_when_not_ready() {
        // The actor is not Ready (Indexing): on_find_references must answer with
        // an empty, non-error completion and issue no LSP request.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").expect("write main.rs");

        let (mut actor, captured, stream, mut rx) =
            ready_actor_with_fake_lsp(&root, &path, "fn main() {}", HashMap::new());
        actor.phase = Phase::Indexing;

        actor.on_find_references(
            CodeIntelFindReferencesPayload {
                references_id: 4,
                path,
                version: ProjectFileVersion(1),
                offset: 3,
                include_declaration: true,
            },
            stream,
        );

        let (results, complete) = drain_references(&mut rx);
        assert!(results.is_empty());
        let complete = complete.expect("honest empty completion");
        assert_eq!(complete.references_id, 4);
        assert_eq!(complete.total_references, 0);
        assert!(!complete.cancelled);
        assert!(complete.error.is_none());
        assert!(
            captured.lock().unwrap().is_empty(),
            "no LSP request while indexing"
        );
    }

    // ── M6: large-file progressive delivery ─────────────────────────────────

    #[test]
    fn is_large_file_triggers_on_bytes_or_occurrences() {
        let tuning = ModelTuning {
            large_bytes: 1000,
            large_occurrences: 100,
            chunk_occurrences: 16,
        };
        // Below both bounds → small.
        assert!(!is_large_file(999, 99, &tuning));
        // At/above the byte bound → large (scope unaffected, only pacing).
        assert!(is_large_file(1000, 0, &tuning));
        // At/above the occurrence bound → large.
        assert!(is_large_file(0, 100, &tuning));
    }

    #[test]
    fn bounding_range_spans_indices() {
        let occ = |s: u32, e: u32| CodeIntelOccurrence {
            range: ByteRange { start: s, end: e },
            role: CodeIntelRole::Reference,
            display: String::new(),
            definition: Vec::new(),
        };
        let occurrences = vec![occ(10, 15), occ(0, 5), occ(20, 25)];
        assert_eq!(
            bounding_range(&occurrences, &[0, 1, 2]),
            Some(ByteRange { start: 0, end: 25 })
        );
        assert_eq!(
            bounding_range(&occurrences, &[1]),
            Some(ByteRange { start: 0, end: 5 })
        );
        assert_eq!(bounding_range(&occurrences, &[]), None);
    }

    #[test]
    fn chunk_plan_orders_visible_window_first() {
        let occ = |s: u32, e: u32| CodeIntelOccurrence {
            range: ByteRange { start: s, end: e },
            role: CodeIntelRole::Reference,
            display: String::new(),
            definition: Vec::new(),
        };
        // Five occurrences in document order, chunk size 2 → chunks
        // [0,1], [2,3], [4].
        let occurrences = vec![occ(0, 2), occ(3, 5), occ(6, 8), occ(9, 11), occ(12, 14)];
        // No visible hint → document order, every chunk present.
        let plain = chunk_plan(&occurrences, None, 2);
        assert_eq!(plain, vec![vec![0, 1], vec![2, 3], vec![4]]);

        // Visible window over the last occurrence → its chunk streams first,
        // the rest keep document order. Coverage is still whole-file (every
        // chunk delivered) — ByteRange is pacing, not a gate.
        let prioritized = chunk_plan(&occurrences, Some(ByteRange { start: 12, end: 14 }), 2);
        assert_eq!(prioritized, vec![vec![4], vec![0, 1], vec![2, 3]]);
        let covered: std::collections::BTreeSet<usize> =
            prioritized.iter().flatten().copied().collect();
        assert_eq!(
            covered,
            (0..5).collect(),
            "every occurrence still delivered"
        );
    }

    #[test]
    fn resource_caps_tighten_under_load_and_limited() {
        // In-flight: large tightens vs small; Limited tightens vs Full; never 0.
        assert_eq!(
            inflight_limit(CodeIntelResourceMode::Full, false),
            MAX_INFLIGHT_DEFINITIONS
        );
        assert!(
            inflight_limit(CodeIntelResourceMode::Full, true)
                < inflight_limit(CodeIntelResourceMode::Full, false),
            "a large file uses a tighter in-flight window"
        );
        assert!(
            inflight_limit(CodeIntelResourceMode::Limited, true)
                <= inflight_limit(CodeIntelResourceMode::Full, true),
            "a constrained host is no more eager than Full"
        );
        for large in [false, true] {
            for mode in [
                CodeIntelResourceMode::Full,
                CodeIntelResourceMode::Limited,
                CodeIntelResourceMode::Unavailable,
            ] {
                assert!(
                    inflight_limit(mode, large) >= 1,
                    "resolution must always make progress (converges on whole file)"
                );
            }
        }

        // Batch: large flushes smaller (visible decorations sooner); Limited
        // smaller still — but the final Complete frame always covers the file.
        assert!(
            batch_limit(CodeIntelResourceMode::Full, true)
                < batch_limit(CodeIntelResourceMode::Full, false)
        );
        assert!(
            batch_limit(CodeIntelResourceMode::Limited, true)
                <= batch_limit(CodeIntelResourceMode::Full, true)
        );
    }

    /// A file with five navigable occurrences on five lines (`f0`..`f4`), each a
    /// 2-byte identifier at a known offset. Returns the source uri/text plus the
    /// semanticTokens response for the fake LSP.
    fn five_occurrence_fixture(dir: &std::path::Path) -> (String, String, Value) {
        let text = "f0\nf1\nf2\nf3\nf4\n".to_owned();
        std::fs::write(dir.join("main.rs"), &text).expect("write main.rs");
        let uri = file_uri(&dir.join("main.rs")).unwrap();
        // Groups of 5: deltaLine, deltaStartChar, length, type=0(function), mod.
        let tokens = json!({
            "data": [
                0, 0, 2, 0, 0,
                1, 0, 2, 0, 0,
                1, 0, 2, 0, 0,
                1, 0, 2, 0, 0,
                1, 0, 2, 0, 0,
            ]
        });
        (uri, text, tokens)
    }

    /// The five known occurrence byte ranges from [`five_occurrence_fixture`].
    fn five_occurrence_ranges() -> Vec<ByteRange> {
        vec![
            ByteRange { start: 0, end: 2 },
            ByteRange { start: 3, end: 5 },
            ByteRange { start: 6, end: 8 },
            ByteRange { start: 9, end: 11 },
            ByteRange { start: 12, end: 14 },
        ]
    }

    /// Union of every occurrence range across a set of streamed model frames.
    fn covered_ranges(
        models: &[CodeIntelFileModelPayload],
    ) -> std::collections::BTreeSet<(u32, u32)> {
        models
            .iter()
            .flat_map(|m| m.occurrences.iter())
            .map(|o| (o.range.start, o.range.end))
            .collect()
    }

    #[tokio::test]
    async fn large_file_streams_byte_range_chunks_visible_first_then_complete() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let (uri, text, tokens) = five_occurrence_fixture(dir.path());

        let mut responses = HashMap::new();
        responses.insert("textDocument/semanticTokens/full".to_owned(), tokens);
        responses.insert("textDocument/definition".to_owned(), Value::Null);
        let (requester, _captured) = fake_requester(responses);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let (visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        // The client's visible window covers the LAST occurrence (bytes 12..14),
        // so its chunk must stream first.
        visible_tx
            .send(ByteRange { start: 12, end: 14 })
            .expect("send visible hint");

        let job = ModelJob {
            requester,
            root: root.clone(),
            path: path.clone(),
            version: ProjectFileVersion(1),
            uri,
            text,
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Full,
            // Force the large path on this tiny file via the occurrence bound,
            // and a 2-occurrence chunk so several ByteRange windows stream.
            tuning: ModelTuning {
                large_bytes: usize::MAX,
                large_occurrences: 1,
                chunk_occurrences: 2,
            },
            visible_rx,
            cancel_rx,
            model_failed_tx: None,
        };
        job.run().await;

        let models = drain_models(&mut rx);
        assert!(
            models.len() >= 4,
            "≥3 ByteRange chunks + a final frame: {models:?}"
        );

        // The first frame is the visible chunk, delivered as a transient
        // ByteRange + Partial window over the on-screen occurrence (12..14).
        assert_eq!(
            models[0].model_range,
            CodeIntelModelRange::ByteRange {
                range: ByteRange { start: 12, end: 14 }
            },
            "the visible window streams first"
        );
        assert_eq!(models[0].completeness, CodeIntelCompleteness::Partial);
        assert_eq!(models[0].version, ProjectFileVersion(1));

        // At least one frame is a transient ByteRange window (never a permanent
        // gate), and completeness is only ever Complete on a FullFile frame.
        assert!(
            models
                .iter()
                .any(|m| matches!(m.model_range, CodeIntelModelRange::ByteRange { .. })),
            "large files stream ByteRange chunks"
        );
        for m in &models {
            if m.completeness == CodeIntelCompleteness::Complete {
                assert_eq!(
                    m.model_range,
                    CodeIntelModelRange::FullFile,
                    "Complete is only ever advertised whole-file"
                );
            }
        }

        // The model converges: a final FullFile + Complete frame, and the union
        // of all streamed chunks covers every occurrence (whole-file scope).
        let last = models.last().unwrap();
        assert_eq!(last.model_range, CodeIntelModelRange::FullFile);
        assert_eq!(last.completeness, CodeIntelCompleteness::Complete);
        let expected: std::collections::BTreeSet<(u32, u32)> = five_occurrence_ranges()
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect();
        assert_eq!(
            covered_ranges(&models),
            expected,
            "ByteRange chunks accumulate to whole-file coverage — nothing dropped"
        );
    }

    #[tokio::test]
    async fn limited_mode_still_converges_to_full_file_complete() {
        // A constrained host (Limited) reduces eagerness (smaller in-flight /
        // batch — asserted in `resource_caps_tighten_under_load_and_limited`) but
        // the model still converges on the whole file: every occurrence is
        // delivered and resolved, ending in a FullFile + Complete frame.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let (uri, text, tokens) = five_occurrence_fixture(dir.path());

        // Definition resolves to a real in-root target so we can prove resolution
        // still proceeds under Limited (just paced).
        let lib_text = "pub fn helper() {}\n";
        std::fs::write(dir.path().join("lib.rs"), lib_text).expect("write lib.rs");
        let lib_uri = file_uri(&dir.path().join("lib.rs")).unwrap();
        let mut responses = HashMap::new();
        responses.insert("textDocument/semanticTokens/full".to_owned(), tokens);
        responses.insert(
            "textDocument/definition".to_owned(),
            json!({
                "uri": lib_uri,
                "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}},
            }),
        );
        let (requester, _captured) = fake_requester(responses);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let (_visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let job = ModelJob {
            requester,
            root: root.clone(),
            path: path.clone(),
            version: ProjectFileVersion(1),
            uri,
            text,
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Limited,
            tuning: ModelTuning {
                large_bytes: usize::MAX,
                large_occurrences: 1,
                chunk_occurrences: 2,
            },
            visible_rx,
            cancel_rx,
            model_failed_tx: None,
        };
        job.run().await;

        let models = drain_models(&mut rx);
        // Converged to whole-file Complete.
        let last = models.last().expect("a final frame");
        assert_eq!(last.model_range, CodeIntelModelRange::FullFile);
        assert_eq!(last.completeness, CodeIntelCompleteness::Complete);
        // Every occurrence delivered (whole-file scope preserved under Limited).
        let expected: std::collections::BTreeSet<(u32, u32)> = five_occurrence_ranges()
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect();
        assert_eq!(covered_ranges(&models), expected);
        // Resolution still happened — at least one streamed occurrence carries
        // its definition target.
        assert!(
            models
                .iter()
                .flat_map(|m| m.occurrences.iter())
                .any(|o| !o.definition.is_empty()),
            "Limited mode still resolves definitions, just less eagerly"
        );
    }

    #[tokio::test]
    async fn large_file_model_cancelled_mid_stream_emits_no_complete() {
        // Supersession / unsubscribe cancels chunked delivery too: after the
        // occurrence-set ByteRange chunks stream, the driver parks on the
        // (delayed) definition requests; cancelling there must stop it before any
        // Complete frame is emitted.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = ProjectRootPath(dir.path().to_string_lossy().into_owned());
        let path = ProjectPath {
            root: root.clone(),
            relative_path: "main.rs".to_owned(),
        };
        let (uri, text, tokens) = five_occurrence_fixture(dir.path());

        // semanticTokens answers immediately; definition only after a long delay,
        // so cancellation reliably wins before any resolution frame.
        let (c2s_w, c2s_r) = tokio::io::duplex(64 * 1024);
        let (s2c_w, s2c_r) = tokio::io::duplex(64 * 1024);
        tokio::spawn(delayed_definition_fake(
            c2s_r,
            s2c_w,
            tokens,
            Duration::from_millis(500),
        ));
        let (client, _events) = LspClient::from_io(c2s_w, s2c_r, None);
        let requester = client.requester();
        std::mem::forget(client);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = Stream::new(StreamPath("/project/p".to_owned()), tx);
        let (_visible_tx, visible_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let job = ModelJob {
            requester,
            root: root.clone(),
            path: path.clone(),
            version: ProjectFileVersion(1),
            uri,
            text,
            provider: CodeIntelProviderId("rust-analyzer".to_owned()),
            language: CodeIntelLanguageId("rust".to_owned()),
            output,
            legend: legend(&["function"], &[]),
            resource_mode: CodeIntelResourceMode::Full,
            tuning: ModelTuning {
                large_bytes: usize::MAX,
                large_occurrences: 1,
                chunk_occurrences: 2,
            },
            visible_rx,
            cancel_rx,
            model_failed_tx: None,
        };
        let handle = tokio::spawn(job.run());

        // The first ByteRange chunk arrives (occurrence set; no resolution needed),
        // proving the chunked stream started.
        let first: CodeIntelFileModelPayload =
            recv_frame(&mut rx, FrameKind::CodeIntelFileModel).await;
        assert!(matches!(
            first.model_range,
            CodeIntelModelRange::ByteRange { .. }
        ));
        assert_eq!(first.completeness, CodeIntelCompleteness::Partial);

        // Cancel while definitions are still "computing".
        drop(cancel_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("driver returns promptly on cancel")
            .expect("driver task joins");

        // Past the definition delay: no Complete (and no FullFile) frame may be
        // emitted after cancellation of the chunked delivery.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let late = drain_models(&mut rx);
        assert!(
            !late
                .iter()
                .any(|m| m.completeness == CodeIntelCompleteness::Complete),
            "a cancelled chunked delivery emits no Complete frame: {late:?}"
        );
    }
}
