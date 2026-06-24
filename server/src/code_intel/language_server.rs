//! The per-language configuration consumed by the generic LSP-backed provider.
//!
//! This is the seam that makes code intelligence **language-agnostic** (spec
//! §M7). ALL behavior — the LSP client lifecycle, `publishDiagnostics` →
//! `code_intel_diagnostics`, the pushed whole-file definition model + `ModelJob`,
//! hover, find-references, byte↔UTF-16 conversion, versioning / `didChange`,
//! large-file progressive delivery, resource mode, status, and crash/restart —
//! lives in [`LspProvider`](super::lsp_provider::LspProvider) /
//! `LspActor` and is reused **unchanged** for every language.
//!
//! A language contributes only a [`LanguageServerConfig`]: its identifiers, the
//! file extensions and workspace-root markers that select it, how to discover
//! the backing binary, and the LSP `initializationOptions`. Adding a language is
//! one new config + one `Language` match arm + an extension/marker mapping —
//! with **no protocol change and no frontend change** (the wire ids are the open
//! [`CodeIntelLanguageId`] / [`CodeIntelProviderId`] string newtypes, which the
//! frontend renders as opaque labels).

use std::path::{Path, PathBuf};

use protocol::{CodeIntelLanguageId, CodeIntelProviderId};
use serde_json::Value;

/// Result of discovering a language server's backing binary.
///
/// v1 is **detect-and-hint only** (spec §2.6): no bundled binary, no managed
/// download. A managed download (fetch a pinned server into `~/.tyde/...`, with
/// a `confirm_dialog` prompt) and remote/SSH spawning are deferred (spec §9);
/// this enum is the hook where a future `Download { … }` variant would slot in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerDiscovery {
    /// A usable server binary plus the args that put it in stdio LSP mode
    /// (empty for rust-analyzer, `["--stdio"]` for pyright-langserver).
    Found { binary: PathBuf, args: Vec<String> },
    /// No usable binary found; `message` is surfaced to the user, `hint` is the
    /// install instruction, and any probe details explain why a discovered
    /// candidate was rejected.
    Absent {
        message: String,
        hint: String,
        exit_status: Option<String>,
        stderr: Option<String>,
    },
}

impl ServerDiscovery {
    pub(crate) fn absent_install(provider: &str, hint: &str) -> Self {
        Self::Absent {
            message: format!("{provider} not installed — run `{hint}`"),
            hint: hint.to_owned(),
            exit_status: None,
            stderr: None,
        }
    }
}

/// Everything a language contributes to the generic LSP-backed provider.
///
/// The generic [`LspProvider`](super::lsp_provider::LspProvider) is parameterized
/// by this; nothing language-specific leaks into the shared machinery.
#[derive(Clone)]
pub(crate) struct LanguageServerConfig {
    /// Open wire language id, e.g. `"rust"`, `"python"`. Rendered opaquely by the
    /// frontend; never a closed enum on the wire.
    pub language: CodeIntelLanguageId,
    /// Open wire provider id, e.g. `"rust-analyzer"`, `"pyright"`.
    pub provider_id: CodeIntelProviderId,
    /// The LSP `languageId` sent in `textDocument/didOpen`.
    pub lsp_language_id: &'static str,
    /// File extensions this language owns (lowercase, no leading dot).
    pub extensions: &'static [&'static str],
    /// Workspace-root marker filenames (e.g. `Cargo.toml`, `pyproject.toml`). A
    /// file is selected for this language only when its extension matches **and**
    /// one of these markers is present at the project root (extension + marker,
    /// §M7 detection) — so a stray `.py` outside a Python project reads as
    /// `Unsupported` rather than spinning up a server.
    pub workspace_markers: &'static [&'static str],
    /// Binary discovery (PATH → language-specific fallback). May shell out, so
    /// the actor runs it via `spawn_blocking`. The workspace root lets rustup
    /// respect a project-local/default toolchain instead of hard-coding one.
    pub discover: fn(&Path) -> ServerDiscovery,
    /// `initializationOptions` for the LSP `initialize` request.
    pub initialization_options: fn() -> Value,
}
