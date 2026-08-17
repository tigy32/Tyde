//! Server-owned code intelligence (`dev-docs/24-code-intelligence.md`).
//!
//! Layers: the per-project-root `CodeIntelService` actor (mirroring
//! `ProjectStreamHandle`), a thin per-project [`CodeIntelRouter`], and — behind
//! the [`provider::CodeIntelProvider`] trait — the generic
//! [`lsp_provider::LspProvider`] engine. That engine is **language-agnostic**
//! (§M7): every behavior (diagnostics, go-to-def, hover, find-references,
//! versioning, large-file delivery, crash/restart) lives there once, and each
//! language contributes only a [`language_server::LanguageServerConfig`]
//! ([`rust_analyzer`] for rust-analyzer, [`pyright`] for Python).
//!
//! Provider selection is server-side only: extension + workspace-root marker
//! ([`detect_language`]) picks the internal [`Language`], whose exhaustive
//! `match`es force every new language to supply a config. This enum **never
//! appears on the wire**; the wire uses the open string newtypes
//! `CodeIntelLanguageId` / `CodeIntelProviderId`, so adding a language costs no
//! protocol change and no frontend change.

mod bootstrap;
mod language_server;
mod lsp_client;
mod lsp_codec;
mod lsp_position;
mod lsp_provider;
mod provider;
mod pyright;
mod rust_analyzer;
mod service;

pub(crate) use service::CodeIntelRouter;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use protocol::{CodeIntelResourceMode, FrameKind, ProjectPath};
use serde::Serialize;
use settings_model::CodeIntelSettings;

use language_server::LanguageServerConfig;

use crate::stream::Stream;

/// The host's code-intelligence **resource mode** (spec §3, §M6) — the single
/// host-capability variable in this design. Local and remote are otherwise
/// identical, with **no transport branching** (`01-philosophy.md` §3): a
/// constrained host advertises [`CodeIntelResourceMode::Limited`], which changes
/// only the *pace and eagerness* of progressive model delivery (smaller batches,
/// fewer in-flight definition requests, visible window resolved first), **never**
/// the final whole-file scope. The model still converges on the whole file.
///
/// No host exposes a constraint signal yet, so this defaults to `Full`. It is
/// the **hook**: when a real signal exists (a host setting, a measured
/// constraint), compute it here and the rest of the pipeline already threads the
/// value through to `code_intel_status` frames and the model-push driver.
pub(crate) fn host_resource_mode() -> CodeIntelResourceMode {
    CodeIntelResourceMode::Full
}

/// The closed, server-side set of languages with a code-intelligence provider.
/// This enum **never appears on the wire** (the wire uses the open string
/// newtypes), so adding a language — one new variant here, one [`config`] arm,
/// and an extension/marker entry on the [`LanguageServerConfig`] — costs **no
/// protocol change and no frontend change**. The `match`es below are exhaustive,
/// so the compiler forces every new variant to be handled (spec §2.3).
///
/// `Hash` so the per-root service can key one generic provider per language
/// (`Rust` and `Python` files in the same root each get their own server).
///
/// [`config`]: Language::config
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Language {
    Rust,
    Python,
}

impl Language {
    /// Every language with a provider. The `config()` match is exhaustive, so a
    /// new variant must be added here too — the single registry detection walks.
    const ALL: [Language; 2] = [Language::Rust, Language::Python];

    pub(crate) fn all() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter()
    }

    /// The per-language [`LanguageServerConfig`] driving the generic provider.
    /// The one place a `Language` resolves to its config (ids, extensions,
    /// markers, discovery, init options); the compiler forces a new variant to
    /// supply one. Everything else here reads *from* the config, so the config
    /// is the single source of truth per language (no duplicated tables).
    pub(crate) fn config(self, settings: &CodeIntelSettings) -> LanguageServerConfig {
        match self {
            Language::Rust => rust_analyzer::rust_config(settings),
            Language::Python => pyright::pyright_config(settings),
        }
    }

    /// Map a file extension (no leading dot) to its language by consulting each
    /// language's `config().extensions` — no separate extension table to drift.
    /// `None` when no provider owns it.
    pub(crate) fn from_extension(extension: &str) -> Option<Self> {
        Language::ALL.into_iter().find(|language| {
            language
                .config(&CodeIntelSettings::default())
                .extensions
                .contains(&extension)
        })
    }

    /// Map a file to its language by extension alone. `None` means no provider
    /// supports this file type. Marker confirmation is [`detect_language`].
    pub(crate) fn from_path(path: &ProjectPath) -> Option<Self> {
        let extension = Path::new(&path.relative_path).extension()?.to_str()?;
        Self::from_extension(extension)
    }

    /// Workspace-root marker filenames that, alongside the extension, confirm a
    /// file belongs to this language's kind of project (extension + marker,
    /// §M7 detection). Sourced from the config.
    pub(crate) fn workspace_markers(self) -> &'static [&'static str] {
        self.config(&CodeIntelSettings::default()).workspace_markers
    }
}

/// Detect a file's language from **extension + a workspace-root marker** (spec
/// §M7): the extension picks the candidate language, and one of that language's
/// markers must be present among `root_entries` (the file names directly under
/// the project root). A `.rs` outside a Cargo project, or a `.py` outside a
/// Python project, is `None` (`Unsupported`) rather than spinning up a server
/// for a stray file. Pure (no filesystem) so it is directly unit-testable; the
/// service reads `root_entries` once per root and calls this.
pub(crate) fn detect_language(
    relative_path: &str,
    root_entries: &HashSet<String>,
) -> Option<Language> {
    let extension = Path::new(relative_path).extension()?.to_str()?;
    let language = Language::from_extension(extension)?;
    language
        .workspace_markers()
        .iter()
        .any(|marker| root_entries.contains(*marker))
        .then_some(language)
}

pub(crate) fn detect_project_languages(root_entries: &HashSet<String>) -> Vec<Language> {
    Language::all()
        .filter(|language| {
            language
                .workspace_markers()
                .iter()
                .any(|marker| root_entries.contains(*marker))
        })
        .collect()
}

/// The absolute on-disk path of a project file: `root` is already an absolute
/// filesystem path (see `project_stream::read_file`, which joins them the same
/// way), so this matches what the rest of the server reads.
pub(crate) fn absolute_path(path: &ProjectPath) -> PathBuf {
    Path::new(&path.root.0).join(&path.relative_path)
}

/// Serialize and push one frame onto a project output stream. A serialization
/// failure is logged (it indicates a protocol bug), never silently swallowed.
/// Shared by the service and every provider.
pub(crate) fn emit<T: Serialize>(output: &Stream, kind: FrameKind, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(value) => {
            let _ = output.send_value(kind, value);
        }
        Err(error) => {
            tracing::error!(%error, %kind, "failed to serialize code-intel frame");
        }
    }
}
