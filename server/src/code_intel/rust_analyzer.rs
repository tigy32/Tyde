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

use super::bootstrap;
use super::language_server::LanguageServerConfig;

/// The rust-analyzer config consumed by [`LspProvider`](super::lsp_provider::LspProvider).
pub(crate) fn rust_config() -> LanguageServerConfig {
    LanguageServerConfig {
        language: CodeIntelLanguageId("rust".to_owned()),
        provider_id: CodeIntelProviderId("rust-analyzer".to_owned()),
        lsp_language_id: "rust",
        extensions: &["rs"],
        workspace_markers: &["Cargo.toml"],
        discover: bootstrap::discover_rust_analyzer,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::{
        CodeIntelDiagnosticsPayload, CodeIntelSeverity, FrameKind, ProjectFileVersion, ProjectPath,
        ProjectRootPath, StreamPath,
    };
    use tokio::sync::mpsc;

    use super::super::bootstrap::{self};
    use super::super::language_server::ServerDiscovery;
    use super::super::lsp_provider::LspProvider;
    use super::super::provider::CodeIntelProvider;
    use super::rust_config;
    use crate::stream::Stream;
    use protocol::CodeIntelResourceMode;

    #[test]
    fn rust_config_identifies_as_rust_analyzer() {
        let config = rust_config();
        assert_eq!(config.language.0, "rust");
        assert_eq!(config.provider_id.0, "rust-analyzer");
        assert_eq!(config.lsp_language_id, "rust");
        assert_eq!(config.extensions, &["rs"]);
        assert_eq!(config.workspace_markers, &["Cargo.toml"]);
    }

    /// rust-analyzer-gated end-to-end test, now driving the **generic**
    /// [`LspProvider`] with the rust config: spin up a real RA over a tiny Cargo
    /// project with a deliberate type error and assert an error diagnostic
    /// eventually arrives. Skips-with-log when RA is not installed — never a
    /// silent pass, never a hard failure on a machine without RA.
    #[tokio::test]
    async fn rust_analyzer_emits_diagnostics_for_broken_file() {
        if matches!(
            bootstrap::discover_rust_analyzer(),
            ServerDiscovery::Absent { .. }
        ) {
            eprintln!(
                "SKIP rust_analyzer_emits_diagnostics_for_broken_file: rust-analyzer not found"
            );
            return;
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"tyde_ci_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    let _x: i32 = \"not an int\";\n}\n",
        )
        .expect("write main.rs");

        let root_path = ProjectRootPath(root.to_string_lossy().into_owned());
        let mut provider = LspProvider::new(
            rust_config(),
            root_path.clone(),
            CodeIntelResourceMode::Full,
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/ci".to_owned()), tx);
        let path = ProjectPath {
            root: root_path,
            relative_path: "src/main.rs".to_owned(),
        };
        provider.subscribe(path, ProjectFileVersion(1), stream);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no error diagnostics within timeout");
            let envelope = match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(envelope)) => envelope,
                Ok(None) => panic!("output stream closed before diagnostics arrived"),
                Err(_) => panic!("timed out waiting for diagnostics"),
            };
            if envelope.kind != FrameKind::CodeIntelDiagnostics {
                continue;
            }
            let payload: CodeIntelDiagnosticsPayload =
                serde_json::from_value(envelope.payload).expect("diagnostics payload");
            if payload
                .diagnostics
                .iter()
                .any(|d| d.severity == CodeIntelSeverity::Error)
            {
                return; // success
            }
        }
    }
}
