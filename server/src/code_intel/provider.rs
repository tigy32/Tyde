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

    /// Stop the provider actor and any backing language-server process. This is
    /// explicit because provider actors can own self-senders for internal timers;
    /// dropping the external handle is not a reliable shutdown signal.
    fn shutdown(&mut self);

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
