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
    CodeIntelProviderStatus, CodeIntelReferencesCompletePayload, CodeIntelResourceMode,
    CodeIntelSetVisibleRangePayload, CodeIntelSettings, CodeIntelState, CodeIntelStatusPayload,
    CodeIntelStatusScope, CodeIntelSubscribeFilePayload, FrameKind, ProjectFileVersion,
    ProjectPath, ProjectRootPath,
};
use tokio::sync::mpsc;

use super::lsp_provider::LspProvider;
use super::provider::CodeIntelProvider;
use super::{Language, detect_language, detect_project_languages, emit, host_resource_mode};
use crate::project_stream::{FileVersionChange, ProjectStreamHandle};
use crate::stream::Stream;

/// Commands sent to a per-root [`CodeIntelService`] actor. These mirror the
/// `CodeIntel*` input frames. Each command that produces output carries the
/// project output `Stream` to push on.
enum CodeIntelCommand {
    Shutdown,
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
    Warm,
    UpdateSettings {
        settings: CodeIntelSettings,
    },
}

/// Clonable handle to a per-root service actor. Shutdown is explicit because
/// providers below the service can own self-senders for internal timers.
#[derive(Clone)]
struct CodeIntelServiceHandle {
    tx: mpsc::UnboundedSender<CodeIntelCommand>,
}

impl CodeIntelServiceHandle {
    fn shutdown(&self) {
        let _ = self.tx.send(CodeIntelCommand::Shutdown);
    }
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
    provider_status_tx: mpsc::UnboundedSender<CodeIntelProviderStatus>,
    version_listener_registered: bool,
    subscriptions: HashSet<ProjectPath>,
    /// The host's resource mode (spec §M6). Passed to each provider so progressive
    /// delivery paces itself, and stamped on every status frame (including the
    /// `Unsupported` path below) so the UI can reflect it.
    resource_mode: CodeIntelResourceMode,
    code_intel_settings: CodeIntelSettings,
}

impl CodeIntelService {
    fn spawn(
        root: ProjectRootPath,
        project_handle: ProjectStreamHandle,
        resource_mode: CodeIntelResourceMode,
        code_intel_settings: CodeIntelSettings,
    ) -> CodeIntelServiceHandle {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (version_tx, version_rx) = mpsc::unbounded_channel();
        let (provider_status_tx, provider_status_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut service = CodeIntelService {
                root,
                providers: HashMap::new(),
                project_handle,
                version_listener_tx: version_tx,
                provider_status_tx,
                version_listener_registered: false,
                subscriptions: HashSet::new(),
                resource_mode,
                code_intel_settings,
            };
            let mut version_rx = Some(version_rx);
            let mut provider_status_rx = Some(provider_status_rx);
            loop {
                tokio::select! {
                    command = rx.recv() => {
                        match command {
                            Some(CodeIntelCommand::Shutdown) => break,
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
                    status = recv_provider_status(&mut provider_status_rx) => {
                        match status {
                            Some(status) => service.handle_provider_status(status),
                            None => provider_status_rx = None,
                        }
                    }
                }
            }
            service.shutdown_providers();
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

    fn handle_provider_status(&self, status: CodeIntelProviderStatus) {
        if let Err(error) = self
            .project_handle
            .update_code_intel_provider_status(self.root.clone(), status)
        {
            tracing::warn!(
                root = %self.root,
                error = %error,
                "failed to publish code-intel overview provider status"
            );
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

    fn provider_for_language(&mut self, language: Language) -> &mut Box<dyn CodeIntelProvider> {
        let root = self.root.clone();
        let resource_mode = self.resource_mode;
        let config = language.config(&self.code_intel_settings);
        let provider_status_tx = self.provider_status_tx.clone();
        self.providers.entry(language).or_insert_with(|| {
            Box::new(LspProvider::with_status_updates(
                config,
                root,
                resource_mode,
                Some(provider_status_tx),
            ))
        })
    }

    fn warm_project_root(&mut self) {
        let root_entries = read_root_entries(&self.root);
        for language in detect_project_languages(&root_entries) {
            self.provider_for_language(language).warm();
        }
    }

    fn shutdown_providers(&mut self) {
        for provider in self.providers.values_mut() {
            provider.shutdown();
        }
        self.providers.clear();
        self.subscriptions.clear();
    }

    async fn handle(&mut self, command: CodeIntelCommand) {
        match command {
            CodeIntelCommand::Shutdown => {}
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
                        self.subscriptions.insert(path.clone());
                        let provider = self.provider_for_language(language);
                        tracing::debug!(
                            provider = %provider.provider_id(),
                            root = %root,
                            ?language,
                            "code-intel: routing file to provider"
                        );
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
            CodeIntelCommand::Warm => {
                self.warm_project_root();
            }
            CodeIntelCommand::UpdateSettings { settings } => {
                for (language, provider) in &mut self.providers {
                    if language_server_path_changed(*language, &self.code_intel_settings, &settings)
                    {
                        provider.reconfigure(language.config(&settings));
                    }
                }
                self.code_intel_settings = settings;
            }
        }
    }
}

fn language_server_path_changed(
    language: Language,
    old_settings: &CodeIntelSettings,
    new_settings: &CodeIntelSettings,
) -> bool {
    let provider_id = language.config(old_settings).provider_id;
    old_settings.language_server_paths.get(&provider_id)
        != new_settings.language_server_paths.get(&provider_id)
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

async fn recv_provider_status(
    rx: &mut Option<mpsc::UnboundedReceiver<CodeIntelProviderStatus>>,
) -> Option<CodeIntelProviderStatus> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending::<Option<CodeIntelProviderStatus>>().await,
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
    code_intel_settings: CodeIntelSettings,
}

impl CodeIntelRouter {
    pub(crate) fn new(
        project_handle: ProjectStreamHandle,
        code_intel_settings: CodeIntelSettings,
    ) -> Self {
        Self {
            services: HashMap::new(),
            project_handle,
            resource_mode: host_resource_mode(),
            code_intel_settings,
        }
    }

    fn service_for(&mut self, root: &ProjectRootPath) -> &CodeIntelServiceHandle {
        let project_handle = self.project_handle.clone();
        let resource_mode = self.resource_mode;
        let code_intel_settings = self.code_intel_settings.clone();
        self.services.entry(root.clone()).or_insert_with(|| {
            CodeIntelService::spawn(
                root.clone(),
                project_handle,
                resource_mode,
                code_intel_settings,
            )
        })
    }

    pub(crate) fn update_settings(&mut self, settings: CodeIntelSettings) {
        self.code_intel_settings = settings.clone();
        for handle in self.services.values() {
            let _ = handle.tx.send(CodeIntelCommand::UpdateSettings {
                settings: settings.clone(),
            });
        }
    }

    pub(crate) fn warm_project(&mut self, roots: Vec<ProjectRootPath>) {
        for root in roots {
            let handle = self.service_for(&root);
            let _ = handle.tx.send(CodeIntelCommand::Warm);
        }
    }

    pub(crate) fn retain_roots(&mut self, roots: &[ProjectRootPath]) {
        let roots = roots.iter().cloned().collect::<HashSet<_>>();
        self.services.retain(|root, handle| {
            let keep = roots.contains(root);
            if !keep {
                handle.shutdown();
            }
            keep
        });
    }

    pub(crate) fn shutdown_all(&mut self) {
        for (_, handle) in self.services.drain() {
            handle.shutdown();
        }
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

impl Drop for CodeIntelRouter {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str) -> ProjectRootPath {
        ProjectRootPath(path.to_owned())
    }

    fn service_handle() -> (
        CodeIntelServiceHandle,
        mpsc::UnboundedReceiver<CodeIntelCommand>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (CodeIntelServiceHandle { tx }, rx)
    }

    fn router_with_services(
        services: HashMap<ProjectRootPath, CodeIntelServiceHandle>,
    ) -> CodeIntelRouter {
        CodeIntelRouter {
            services,
            project_handle: ProjectStreamHandle::disconnected_for_test(),
            resource_mode: CodeIntelResourceMode::Full,
            code_intel_settings: CodeIntelSettings::default(),
        }
    }

    #[test]
    fn retain_roots_shuts_down_retired_services_before_settings_fanout() {
        let live_root = root("/repo/live");
        let retired_root = root("/repo/retired");
        let (live_handle, mut live_rx) = service_handle();
        let (retired_handle, mut retired_rx) = service_handle();
        let mut services = HashMap::new();
        services.insert(live_root.clone(), live_handle);
        services.insert(retired_root.clone(), retired_handle);
        let mut router = router_with_services(services);

        router.retain_roots(std::slice::from_ref(&live_root));

        assert!(router.services.contains_key(&live_root));
        assert!(!router.services.contains_key(&retired_root));
        assert!(matches!(
            retired_rx.try_recv(),
            Ok(CodeIntelCommand::Shutdown)
        ));
        assert!(live_rx.try_recv().is_err());

        router.update_settings(CodeIntelSettings::default());

        assert!(matches!(
            live_rx.try_recv(),
            Ok(CodeIntelCommand::UpdateSettings { .. })
        ));
        assert!(
            retired_rx.try_recv().is_err(),
            "retired services must not receive settings fanout"
        );
    }

    #[test]
    fn shutdown_all_drains_services_and_sends_shutdown() {
        let (first_handle, mut first_rx) = service_handle();
        let (second_handle, mut second_rx) = service_handle();
        let mut services = HashMap::new();
        services.insert(root("/repo/a"), first_handle);
        services.insert(root("/repo/b"), second_handle);
        let mut router = router_with_services(services);

        router.shutdown_all();

        assert!(router.services.is_empty());
        assert!(matches!(
            first_rx.try_recv(),
            Ok(CodeIntelCommand::Shutdown)
        ));
        assert!(matches!(
            second_rx.try_recv(),
            Ok(CodeIntelCommand::Shutdown)
        ));
    }
}
