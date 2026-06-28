//! Code-intelligence provider abstraction.
//!
//! A provider owns the actual semantic resolution for one project root. Unlike
//! M0 (where `subscribe` returned a single status+model synchronously), a
//! provider now **pushes frames onto the project output stream over time**:
//! status transitions (`Starting → Indexing → Ready`) and unsolicited
//! `code_intel_diagnostics` snapshots all arrive asynchronously. The trait is
//! therefore a fire-and-forget push API — the real
//! [`LspProvider`](super::lsp_provider::LspProvider) (driven per-language by a
//! `LanguageServerConfig`) is an actor behind it, and the test-only
//! [`MockProvider`] emits synchronously.

use protocol::{
    CodeIntelFindReferencesPayload, CodeIntelHoverPayload, CodeIntelNavigatePayload,
    CodeIntelProviderId, CodeIntelSetVisibleRangePayload, ProjectFileVersion, ProjectPath,
};

use super::language_server::LanguageServerConfig;
use crate::stream::Stream;

/// A code-intelligence provider for one project root. Methods are
/// non-blocking: a provider that needs to do async work (spawn a language
/// server, await a handshake) does it on its own task and pushes frames onto
/// `output` as they resolve.
pub(crate) trait CodeIntelProvider: Send {
    /// Open wire identifier, e.g. `"rust-analyzer"`.
    fn provider_id(&self) -> CodeIntelProviderId;

    /// Replace the language-server configuration for this provider and restart
    /// discovery/spawn for any already-subscribed files.
    fn reconfigure(&mut self, config: LanguageServerConfig);

    /// Start (or refresh) pushing the semantic model + diagnostics for a file
    /// at the given version onto `output`.
    fn subscribe(&mut self, path: ProjectPath, version: ProjectFileVersion, output: Stream);

    /// Start the root/language provider without subscribing a UI file. This is
    /// used for project-level lazy warmup; it must not synthesize `didOpen`.
    fn warm(&mut self);

    /// Stop pushing for a file.
    fn unsubscribe(&mut self, path: &ProjectPath);

    /// The centralized per-file version counter advanced for a file this
    /// provider has subscribed (external edit, branch switch, agent write —
    /// observed via the project watcher, §M4). The provider re-reads the new
    /// contents, syncs them to the language server (`textDocument/didChange`),
    /// and re-pushes the semantic model + diagnostics stamped with `version`,
    /// superseding any in-flight resolution for the old version. Re-uses the
    /// stored per-file output `Stream` — there is no new subscribe. A version
    /// that does not strictly advance the provider's tracked version is a no-op
    /// (monotonic; older is dropped).
    fn file_version_changed(&mut self, path: &ProjectPath, version: ProjectFileVersion);

    /// Reprioritize a file's in-flight whole-file resolution so the visible
    /// byte range resolves first (M3). A **pure hint** — it never gates which
    /// identifiers are clickable, and the model still converges on the whole
    /// file. A provider with no in-flight resolution simply ignores it.
    fn set_visible_range(&mut self, payload: CodeIntelSetVisibleRangePayload);

    /// On-demand go-to-definition (miss-fill). The provider resolves the
    /// definition target(s) at the requested byte offset and pushes a
    /// `code_intel_navigate_result` onto `output`, correlated by the payload's
    /// `navigate_id`. An honest empty `targets` (no definition / provider not
    /// ready) is a valid answer, never a fabricated one.
    fn navigate(&mut self, payload: CodeIntelNavigatePayload, output: Stream);

    /// On-demand hover. Resolves type/doc markdown at the requested byte offset
    /// and pushes a `code_intel_hover_result` correlated by `hover_id`. `None`
    /// contents ("nothing to show here") is a valid answer.
    fn hover(&mut self, payload: CodeIntelHoverPayload, output: Stream);

    /// Streamed find-references (M5). The provider issues
    /// `textDocument/references` at the requested byte offset, groups the
    /// resulting locations by file, and pushes one `code_intel_references_results`
    /// frame per file (each with byte ranges + a per-line preview) followed by a
    /// terminal `code_intel_references_complete`, all correlated by the payload's
    /// `references_id`. Marking this id active **supersedes** any prior in-flight
    /// query (its late frames are dropped). An honest empty (non-error)
    /// completion is the answer when the provider is not ready.
    fn find_references(&mut self, payload: CodeIntelFindReferencesPayload, output: Stream);

    /// Cancel the in-flight find-references query iff `references_id` is still the
    /// active one. A newer query (or an unrelated id) is left untouched. The
    /// cancelled query terminates with a `cancelled: true` completion.
    fn cancel_references(&mut self, references_id: u64);
}

/// Mock provider + its pure resolution logic. Test-only now that the service
/// selects the real rust-analyzer provider for Rust files; retained as the
/// reference implementation of the push contract and to keep the versioned
/// push path exercised without a language server.
#[cfg(test)]
mod mock {
    use super::*;
    use protocol::{
        CodeIntelCompleteness, CodeIntelFileModelPayload, CodeIntelModelRange,
        CodeIntelResourceMode, CodeIntelState, CodeIntelStatusPayload, CodeIntelStatusScope,
        FrameKind,
    };

    use crate::code_intel::{Language, emit};

    /// What the mock resolves for a freshly-subscribed file: a typed status plus
    /// an optional model. A status is always present (cold start must read as a
    /// real state, never a faked empty result); the model is present only when
    /// the file is supported.
    pub(crate) struct ProviderSubscribeOutput {
        pub status: CodeIntelStatusPayload,
        pub model: Option<CodeIntelFileModelPayload>,
    }

    pub(crate) struct MockProvider;

    impl MockProvider {
        /// Pure resolution logic, independent of the stream, so tests can assert
        /// on the value without capturing emitted frames.
        pub(crate) fn resolve(
            &self,
            path: &ProjectPath,
            version: ProjectFileVersion,
        ) -> ProviderSubscribeOutput {
            let scope = CodeIntelStatusScope::File {
                path: path.clone(),
                version,
            };
            match Language::from_path(path) {
                Some(language) => {
                    let status = CodeIntelStatusPayload {
                        scope,
                        state: CodeIntelState::Ready,
                        resource_mode: CodeIntelResourceMode::Full,
                        work_done: None,
                        total_work: None,
                        message: None,
                    };
                    let model = CodeIntelFileModelPayload {
                        path: path.clone(),
                        version,
                        provider: self.provider_id(),
                        language: language.config(&Default::default()).language,
                        model_range: CodeIntelModelRange::FullFile,
                        completeness: CodeIntelCompleteness::Complete,
                        occurrences: Vec::new(),
                    };
                    ProviderSubscribeOutput {
                        status,
                        model: Some(model),
                    }
                }
                None => {
                    let status = CodeIntelStatusPayload {
                        scope,
                        state: CodeIntelState::Unsupported,
                        resource_mode: CodeIntelResourceMode::Full,
                        work_done: None,
                        total_work: None,
                        message: Some(
                            "no code-intelligence provider for this file type".to_owned(),
                        ),
                    };
                    ProviderSubscribeOutput {
                        status,
                        model: None,
                    }
                }
            }
        }
    }

    impl CodeIntelProvider for MockProvider {
        fn provider_id(&self) -> CodeIntelProviderId {
            CodeIntelProviderId("mock".to_owned())
        }

        fn reconfigure(&mut self, _config: LanguageServerConfig) {}

        fn subscribe(&mut self, path: ProjectPath, version: ProjectFileVersion, output: Stream) {
            let out = self.resolve(&path, version);
            emit(&output, FrameKind::CodeIntelStatus, &out.status);
            if let Some(model) = out.model {
                emit(&output, FrameKind::CodeIntelFileModel, &model);
            }
        }

        fn warm(&mut self) {}

        fn unsubscribe(&mut self, _path: &ProjectPath) {}

        fn file_version_changed(&mut self, _path: &ProjectPath, _version: ProjectFileVersion) {
            // The mock is stateless and stores no per-file output stream, so it
            // has nothing to re-push. The real rust-analyzer provider re-reads
            // and re-resolves here (§M4).
        }

        fn set_visible_range(&mut self, _payload: CodeIntelSetVisibleRangePayload) {
            // The mock pushes a Complete model up front, so there is no in-flight
            // resolution to reprioritize.
        }

        fn navigate(&mut self, payload: CodeIntelNavigatePayload, output: Stream) {
            // The mock resolves no targets — an honest empty answer.
            let result = protocol::CodeIntelNavigateResultPayload {
                navigate_id: payload.navigate_id,
                path: payload.path,
                version: payload.version,
                targets: Vec::new(),
            };
            emit(&output, FrameKind::CodeIntelNavigateResult, &result);
        }

        fn hover(&mut self, payload: CodeIntelHoverPayload, output: Stream) {
            let result = protocol::CodeIntelHoverResultPayload {
                hover_id: payload.hover_id,
                path: payload.path,
                version: payload.version,
                contents: None,
                range: None,
            };
            emit(&output, FrameKind::CodeIntelHoverResult, &result);
        }

        fn find_references(&mut self, payload: CodeIntelFindReferencesPayload, output: Stream) {
            // The mock resolves no references — an honest empty, non-error
            // terminal completion.
            let complete = protocol::CodeIntelReferencesCompletePayload {
                references_id: payload.references_id,
                total_files: 0,
                total_references: 0,
                truncated: false,
                cancelled: false,
                error: None,
            };
            emit(&output, FrameKind::CodeIntelReferencesComplete, &complete);
        }

        fn cancel_references(&mut self, _references_id: u64) {
            // The mock has no in-flight query to cancel.
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use protocol::ProjectRootPath;

        fn path(relative: &str) -> ProjectPath {
            ProjectPath {
                root: ProjectRootPath("/repo".to_owned()),
                relative_path: relative.to_owned(),
            }
        }

        #[test]
        fn supported_file_is_ready_with_model() {
            let provider = MockProvider;
            let out = provider.resolve(&path("src/main.rs"), ProjectFileVersion(3));
            assert_eq!(out.status.state, CodeIntelState::Ready);
            let model = out.model.expect("supported file has a model");
            assert_eq!(model.version, ProjectFileVersion(3));
            assert_eq!(
                model.language,
                Language::Rust.config(&Default::default()).language
            );
        }

        #[test]
        fn unsupported_file_is_unsupported_without_model() {
            let provider = MockProvider;
            let out = provider.resolve(&path("notes.txt"), ProjectFileVersion(1));
            assert_eq!(out.status.state, CodeIntelState::Unsupported);
            assert!(out.model.is_none());
        }
    }
}
