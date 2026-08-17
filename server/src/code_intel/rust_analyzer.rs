//! Rust language configuration for the generic LSP provider.
//!
//! After the §M7 refactor this module is **just a config** — the first language
//! over the shared [`LspProvider`](super::lsp_provider::LspProvider) engine. It
//! contributes the rust-analyzer identifiers, the `.rs` extension + `Cargo.toml`
//! marker, binary discovery (see [`super::bootstrap`]), and rust-analyzer's
//! `initializationOptions` (cargo build-scripts + proc-macros). Every behavior —
//! diagnostics, go-to-def, hover, find-references, versioning, large-file
//! delivery, crash/restart — lives in the engine and is reused unchanged.

use protocol::{CodeIntelLanguageId, CodeIntelProviderId};
use serde_json::{Value, json};
use settings_model::CodeIntelSettings;

use super::bootstrap;
use super::language_server::LanguageServerConfig;

/// The rust-analyzer config consumed by [`LspProvider`](super::lsp_provider::LspProvider).
pub(crate) fn rust_config(settings: &CodeIntelSettings) -> LanguageServerConfig {
    let provider_id = CodeIntelProviderId("rust-analyzer".to_owned());
    let configured_path = settings.language_server_paths.get(&provider_id).cloned();
    LanguageServerConfig {
        language: CodeIntelLanguageId("rust".to_owned()),
        provider_id,
        lsp_language_id: "rust",
        extensions: &["rs"],
        workspace_markers: &["Cargo.toml"],
        discover: bootstrap::discover_rust_analyzer,
        configured_path,
        initialization_options: rust_initialization_options,
    }
}

/// Enable cargo build-scripts and proc-macros so diagnostics are complete.
fn rust_initialization_options() -> Value {
    json!({
        "cargo": { "buildScripts": { "enable": true } },
        "procMacro": { "enable": true }
    })
}
