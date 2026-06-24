//! The `CodeIntelService` actor (per project root) and the thin per-project
//! [`CodeIntelRouter`].
//!
//! Architecture (mirrors `ProjectStreamHandle`):
//!
//! ```text
//! CodeIntelRouter (per project, thin: maps ProjectPath -> root, no provider state)
//!     └── CodeIntelService actor (per root, authoritative, owns a provider)
//!             └── provider (M0: MockProvider; M1+: rust-analyzer subprocess)
//! ```
//!
//! All output frames are pushed onto the project's `/project/<id>` output
//! stream via `send_value`, copying the `search_project_files` pattern.

use std::collections::{HashMap, HashSet};

use protocol::{
    CodeIntelCancelReferencesPayload, CodeIntelErrorCode, CodeIntelErrorContext,
    CodeIntelErrorPayload, CodeIntelFindReferencesPayload, CodeIntelHoverPayload,
    CodeIntelHoverResultPayload, CodeIntelNavigatePayload, CodeIntelNavigateResultPayload,
    CodeIntelReferencesCompletePayload, CodeIntelResourceMode, CodeIntelSetVisibleRangePayload,
    CodeIntelState, CodeIntelStatusPayload, CodeIntelStatusScope, CodeIntelSubscribeFilePayload,
    FrameKind, ProjectFileVersion, ProjectPath, ProjectRootPath,
};
use tokio::sync::mpsc;

use super::lsp_provider::LspProvider;
use super::provider::CodeIntelProvider;
use super::{Language, detect_language, emit, host_resource_mode};
use crate::project_stream::{FileVersionChange, ProjectStreamHandle};
use crate::stream::Stream;

/// Commands sent to a per-root [`CodeIntelService`] actor. These mirror the
/// `CodeIntel*` input frames. Each command that produces output carries the
/// project output `Stream` to push on.
enum CodeIntelCommand {
    Subscribe {
        path: ProjectPath,
        output: Stream,
    },
    Unsubscribe {
        path: ProjectPath,
    },
    SetVisibleRange {
        payload: CodeIntelSetVisibleRangePayload,
    },
    Hover {
        payload: CodeIntelHoverPayload,
        output: Stream,
    },
    Navigate {
        payload: CodeIntelNavigatePayload,
        output: Stream,
    },
    FindReferences {
        payload: CodeIntelFindReferencesPayload,
        output: Stream,
    },
    CancelReferences {
        payload: CodeIntelCancelReferencesPayload,
    },
}

/// Clonable handle to a per-root service actor. Dropping every handle closes
/// the command channel and the actor task exits.
#[derive(Clone)]
struct CodeIntelServiceHandle {
    tx: mpsc::UnboundedSender<CodeIntelCommand>,
}

/// The authoritative per-root code-intelligence service. Owns the root's
/// provider (lazily spawned on the first supported file) and the set of
/// subscribed files for this root.
struct CodeIntelService {
    root: ProjectRootPath,
    /// One generic [`LspProvider`] per language seen in this root, lazily spawned
    /// on the first file of that language and keyed by the internal [`Language`]
    /// (§M7). Rust and Python files in the same root each get their own provider
    /// over the **same** engine — adding a language adds a map entry, not a code
    /// path. One instance per (root, language); each handles its own multi-crate
    /// / multi-package workspace.
    providers: HashMap<Language, Box<dyn CodeIntelProvider>>,
    project_handle: ProjectStreamHandle,
    version_listener_tx: mpsc::UnboundedSender<FileVersionChange>,
    version_listener_registered: bool,
    subscriptions: HashSet<ProjectPath>,
    /// The host's resource mode (spec §M6). Passed to each provider so progressive
    /// delivery paces itself, and stamped on every status frame (including the
    /// `Unsupported` path below) so the UI can reflect it.
    resource_mode: CodeIntelResourceMode,
}

impl CodeIntelService {
    fn spawn(
        root: ProjectRootPath,
        project_handle: ProjectStreamHandle,
        resource_mode: CodeIntelResourceMode,
    ) -> CodeIntelServiceHandle {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (version_tx, version_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut service = CodeIntelService {
                root,
                providers: HashMap::new(),
                project_handle,
                version_listener_tx: version_tx,
                version_listener_registered: false,
                subscriptions: HashSet::new(),
                resource_mode,
            };
            let mut version_rx = Some(version_rx);
            loop {
                tokio::select! {
                    command = rx.recv() => {
                        match command {
                            Some(command) => service.handle(command).await,
                            None => break, // all handles dropped
                        }
                    }
                    change = recv_version_change(&mut version_rx) => {
                        match change {
                            Some(change) => service.handle_version_change(change),
                            // The project-stream actor dropped its listener
                            // sender (the project subscription ended). Stop
                            // polling the dead channel; keep serving commands.
                            None => version_rx = None,
                        }
                    }
                }
            }
        });
        CodeIntelServiceHandle { tx }
    }

    async fn subscribe_version(
        &mut self,
        path: &ProjectPath,
        output: &Stream,
    ) -> Option<ProjectFileVersion> {
        let result = if self.version_listener_registered {
            self.project_handle.current_file_version(path.clone()).await
        } else {
            self.project_handle
                .register_file_version_listener_and_current_version(
                    path.clone(),
                    self.version_listener_tx.clone(),
                )
                .await
        };
        match result {
            Ok(version) => {
                self.version_listener_registered = true;
                Some(version)
            }
            Err(error) => {
                let payload = CodeIntelErrorPayload {
                    code: CodeIntelErrorCode::Internal,
                    message: format!(
                        "failed to resolve subscribe-time file version for {}: {error}",
                        path.relative_path
                    ),
                    hint: None,
                    exit_status: None,
                    stderr: None,
                    context: CodeIntelErrorContext::Subscribe { path: path.clone() },
                    fatal: false,
                };
                emit(output, FrameKind::CodeIntelError, &payload);
                None
            }
        }
    }

    /// A per-file version bump arrived from the project-stream actor (§M4).
    /// Only files this root has actually subscribed are re-resolved — other
    /// files in the project (and files belonging to other roots) are ignored.
    /// The provider enforces monotonicity, so a stale/duplicate version is a
    /// no-op there.
    fn handle_version_change(&mut self, change: FileVersionChange) {
        if !self.subscriptions.contains(&change.path) {
            return;
        }
        if let Some(provider) = self.provider_for_path(&change.path) {
            provider.file_version_changed(&change.path, change.version);
        }
    }

    /// The already-spawned provider serving `path`'s language, if any. Routing
    /// an on-demand query (hover/navigate/references) only needs to reach an
    /// existing provider — extension is enough since a provider exists only
    /// because a marker-confirmed file of that language was subscribed.
    fn provider_for_path(&mut self, path: &ProjectPath) -> Option<&mut Box<dyn CodeIntelProvider>> {
        let language = Language::from_path(path)?;
        self.providers.get_mut(&language)
    }

    async fn handle(&mut self, command: CodeIntelCommand) {
        match command {
            CodeIntelCommand::Subscribe { path, output } => {
                let Some(version) = self.subscribe_version(&path, &output).await else {
                    return;
                };
                // Provider selection is server-side only and language-agnostic:
                // detect by extension + workspace-root marker, then resolve the
                // closed `Language` to its `LanguageServerConfig` and run it on
                // the one generic engine. A new language is a new `Language`
                // variant + config — no protocol or frontend change.
                let root_entries = read_root_entries(&self.root);
                match detect_language(&path.relative_path, &root_entries) {
                    Some(language) => {
                        let root = self.root.clone();
                        let resource_mode = self.resource_mode;
                        let provider = self.providers.entry(language).or_insert_with(|| {
                            Box::new(LspProvider::new(language.config(), root, resource_mode))
                        });
                        tracing::debug!(
                            provider = %provider.provider_id(),
                            root = %self.root,
                            ?language,
                            "code-intel: routing file to provider"
                        );
                        self.subscriptions.insert(path.clone());
                        provider.subscribe(path, version, output);
                    }
                    None => {
                        // No provider matches this file (unknown extension, or a
                        // known one without its project marker): an honest
                        // `Unsupported` status, never a silent empty success.
                        let status = CodeIntelStatusPayload {
                            scope: CodeIntelStatusScope::File { path, version },
                            state: CodeIntelState::Unsupported,
                            resource_mode: self.resource_mode,
                            work_done: None,
                            total_work: None,
                            message: Some(
                                "no code-intelligence provider for this file type".to_owned(),
                            ),
                        };
                        emit(&output, FrameKind::CodeIntelStatus, &status);
                    }
                }
            }
            CodeIntelCommand::Unsubscribe { path } => {
                if let Some(provider) = self.provider_for_path(&path) {
                    provider.unsubscribe(&path);
                }
                self.subscriptions.remove(&path);
            }
            CodeIntelCommand::SetVisibleRange { payload } => {
                // M3: forward the visible-range hint to the file's provider so it
                // resolves on-screen occurrences first. Pure prioritization — it
                // never gates the whole-file scope. No provider up yet ⇒ nothing
                // in flight to reorder, so drop it.
                if let Some(provider) = self.provider_for_path(&payload.path) {
                    provider.set_visible_range(payload);
                }
            }
            CodeIntelCommand::Hover { payload, output } => {
                // Route to the file's provider if one is up (it was spawned on
                // the first subscribe). If no provider exists yet — or the file
                // is unsupported / not ready — answer honestly empty rather than
                // fabricating a hover (spec §4.2: `None` means "nothing here").
                match self.provider_for_path(&payload.path) {
                    Some(provider) => provider.hover(payload, output),
                    None => {
                        let result = CodeIntelHoverResultPayload {
                            hover_id: payload.hover_id,
                            path: payload.path,
                            version: payload.version,
                            contents: None,
                            range: None,
                        };
                        emit(&output, FrameKind::CodeIntelHoverResult, &result);
                    }
                }
            }
            CodeIntelCommand::Navigate { payload, output } => {
                // Same as hover: delegate to the provider, or an honest empty
                // `targets` ("no definition found here") when none is running.
                match self.provider_for_path(&payload.path) {
                    Some(provider) => provider.navigate(payload, output),
                    None => {
                        let result = CodeIntelNavigateResultPayload {
                            navigate_id: payload.navigate_id,
                            path: payload.path,
                            version: payload.version,
                            targets: Vec::new(),
                        };
                        emit(&output, FrameKind::CodeIntelNavigateResult, &result);
                    }
                }
            }
            CodeIntelCommand::FindReferences { payload, output } => {
                // Delegate to the file's provider if one is up (spawned on the
                // first subscribe). With no provider running — the file was never
                // subscribed, or is unsupported — answer honestly with an empty,
                // non-error completion rather than spinning up a language server
                // just for a stray query. Mirrors the hover/navigate gating.
                match self.provider_for_path(&payload.path) {
                    Some(provider) => provider.find_references(payload, output),
                    None => {
                        let complete = CodeIntelReferencesCompletePayload {
                            references_id: payload.references_id,
                            total_files: 0,
                            total_references: 0,
                            truncated: false,
                            cancelled: false,
                            error: None,
                        };
                        emit(&output, FrameKind::CodeIntelReferencesComplete, &complete);
                    }
                }
            }
            CodeIntelCommand::CancelReferences { payload } => {
                // Cancel carries only a `references_id`, no path, so broadcast to
                // every provider in this root; the one owning the query cancels,
                // the rest no-op.
                for provider in self.providers.values_mut() {
                    provider.cancel_references(payload.references_id);
                }
            }
        }
    }
}

/// The file names directly under a project root, used by [`detect_language`] to
/// confirm a workspace-root marker (extension + marker, §M7). Read fresh per
/// subscribe (subscribes are infrequent and "do not cache by default"). An
/// unreadable root yields an empty set, so detection honestly reports
/// `Unsupported` rather than guessing.
fn read_root_entries(root: &ProjectRootPath) -> HashSet<String> {
    let mut entries = HashSet::new();
    if let Ok(read_dir) = std::fs::read_dir(&root.0) {
        for entry in read_dir.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                entries.insert(name.to_owned());
            }
        }
    }
    entries
}

/// Await the next per-file version change, or pend forever once the project
/// stream's listener channel has closed (so a closed channel never busy-loops
/// the actor's `select!`).
async fn recv_version_change(
    rx: &mut Option<mpsc::UnboundedReceiver<FileVersionChange>>,
) -> Option<FileVersionChange> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending::<Option<FileVersionChange>>().await,
    }
}

/// Thin per-project router. Holds no provider state: it maps each `CodeIntel*`
/// frame to the right root (by the frame's `ProjectPath`) and delegates to that
/// root's [`CodeIntelService`] actor, lazily spawning it on first use. It holds
/// the project-stream handle only to hand each spawned service a way to
/// register for version-change notifications (§M4) — it is still plumbing, not
/// provider state.
pub(crate) struct CodeIntelRouter {
    services: HashMap<ProjectRootPath, CodeIntelServiceHandle>,
    project_handle: ProjectStreamHandle,
    /// The host's resource mode (spec §M6), captured once via the
    /// [`host_resource_mode`] hook and handed to every spawned service. Wiring a
    /// real per-host signal later means changing only where this is sourced —
    /// the rest of the pipeline already threads it through.
    resource_mode: CodeIntelResourceMode,
}

impl CodeIntelRouter {
    pub(crate) fn new(project_handle: ProjectStreamHandle) -> Self {
        Self {
            services: HashMap::new(),
            project_handle,
            resource_mode: host_resource_mode(),
        }
    }

    fn service_for(&mut self, root: &ProjectRootPath) -> &CodeIntelServiceHandle {
        let project_handle = self.project_handle.clone();
        let resource_mode = self.resource_mode;
        self.services
            .entry(root.clone())
            .or_insert_with(|| CodeIntelService::spawn(root.clone(), project_handle, resource_mode))
    }

    pub(crate) fn subscribe(&mut self, payload: CodeIntelSubscribeFilePayload, output: Stream) {
        let handle = self.service_for(&payload.path.root);
        let _ = handle.tx.send(CodeIntelCommand::Subscribe {
            path: payload.path,
            output,
        });
    }

    pub(crate) fn unsubscribe(&mut self, path: ProjectPath) {
        let handle = self.service_for(&path.root);
        let _ = handle.tx.send(CodeIntelCommand::Unsubscribe { path });
    }

    pub(crate) fn set_visible_range(&mut self, payload: CodeIntelSetVisibleRangePayload) {
        let handle = self.service_for(&payload.path.root);
        let _ = handle
            .tx
            .send(CodeIntelCommand::SetVisibleRange { payload });
    }

    pub(crate) fn hover(&mut self, payload: CodeIntelHoverPayload, output: Stream) {
        let handle = self.service_for(&payload.path.root);
        let _ = handle.tx.send(CodeIntelCommand::Hover { payload, output });
    }

    pub(crate) fn navigate(&mut self, payload: CodeIntelNavigatePayload, output: Stream) {
        let handle = self.service_for(&payload.path.root);
        let _ = handle
            .tx
            .send(CodeIntelCommand::Navigate { payload, output });
    }

    pub(crate) fn find_references(
        &mut self,
        payload: CodeIntelFindReferencesPayload,
        output: Stream,
    ) {
        let handle = self.service_for(&payload.path.root);
        let _ = handle
            .tx
            .send(CodeIntelCommand::FindReferences { payload, output });
    }

    pub(crate) fn cancel_references(&mut self, payload: CodeIntelCancelReferencesPayload) {
        // Cancel carries only a `references_id`, no path, so it can't pick a
        // root. Broadcast to every spawned service; the one owning the query
        // cancels, the rest no-op.
        for handle in self.services.values() {
            let _ = handle.tx.send(CodeIntelCommand::CancelReferences {
                payload: payload.clone(),
            });
        }
    }
}
