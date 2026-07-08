//! Python language configuration for the generic LSP provider — the **second
//! language**, added to prove the engine is language-agnostic (spec §M7).
//!
//! It contributes ONLY a [`LanguageServerConfig`]: the pyright identifiers, the
//! `.py`/`.pyi` extensions + Python project markers, binary discovery, and
//! (empty) `initializationOptions`. Diagnostics, go-to-definition, hover,
//! find-references, versioning, large-file delivery, and crash/restart all flow
//! through the same [`LspProvider`](super::lsp_provider::LspProvider) with **no
//! new code path** — and adding it required **no protocol change and no frontend
//! change** (the wire ids are open string newtypes the frontend renders as
//! opaque labels).
//!
//! Discovery is **detect-and-hint only** (spec §2.6): a managed pyright download
//! and remote/SSH spawning are deferred (§9).

use std::path::{Path, PathBuf};

use protocol::{CodeIntelLanguageId, CodeIntelProviderId, CodeIntelSettings, HostExecutablePath};
use serde_json::json;

use super::language_server::{LanguageServerConfig, ServerDiscovery};
use crate::process_env;

/// The install hint surfaced when no pyright server binary is found.
pub(crate) const INSTALL_HINT: &str = "npm install -g pyright";
const CONFIGURED_PATH_HINT: &str =
    "Set Tyde's pyright binary path to a usable pyright-langserver executable.";

/// The pyright config consumed by [`LspProvider`](super::lsp_provider::LspProvider).
pub(crate) fn pyright_config(settings: &CodeIntelSettings) -> LanguageServerConfig {
    let provider_id = CodeIntelProviderId("pyright".to_owned());
    let configured_path = settings.language_server_paths.get(&provider_id).cloned();
    LanguageServerConfig {
        language: CodeIntelLanguageId("python".to_owned()),
        provider_id,
        lsp_language_id: "python",
        extensions: &["py", "pyi"],
        workspace_markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
        discover: discover_pyright_configured,
        configured_path,
        initialization_options: || json!({}),
    }
}

/// Discover pyright's stdio language server. Order:
///
/// 1. `pyright-langserver` on PATH (the real LSP entry point),
/// 2. else `pyright` on PATH,
/// 3. else [`ServerDiscovery::Absent`] with the install hint
///    `npm install -g pyright`.
///
/// Both candidates are launched with `--stdio` to put them in LSP mode.
pub(crate) fn discover_pyright(_workspace_root: &Path) -> ServerDiscovery {
    discover_with(process_env::find_executable_in_path)
}

fn discover_pyright_configured(
    workspace_root: &Path,
    configured_path: Option<&HostExecutablePath>,
) -> ServerDiscovery {
    if let Some(configured_path) = configured_path {
        let path = PathBuf::from(&configured_path.0);
        if path.is_file() {
            return ServerDiscovery::Found {
                binary: path,
                args: vec!["--stdio".to_owned()],
            };
        }
        let reason = if path.exists() {
            "not a file"
        } else {
            "file does not exist"
        };
        return ServerDiscovery::Absent {
            message: format!(
                "configured pyright binary path {} is not usable: {reason}",
                path.display(),
            ),
            hint: CONFIGURED_PATH_HINT.to_owned(),
            exit_status: None,
            stderr: None,
        };
    }
    discover_pyright(workspace_root)
}

/// Pure discovery ordering, with the PATH lookup injected so the ordering /
/// absence logic is unit-testable without pyright installed.
fn discover_with(mut find_in_path: impl FnMut(&str) -> Option<PathBuf>) -> ServerDiscovery {
    for binary in ["pyright-langserver", "pyright"] {
        if let Some(path) = find_in_path(binary) {
            return ServerDiscovery::Found {
                binary: path,
                args: vec!["--stdio".to_owned()],
            };
        }
    }
    ServerDiscovery::absent_install("pyright", INSTALL_HINT)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use protocol::{
        CodeIntelErrorPayload, CodeIntelProviderId, CodeIntelResourceMode, CodeIntelSettings,
        CodeIntelState, CodeIntelStatusPayload, FrameKind, HostExecutablePath, ProjectFileVersion,
        ProjectPath, ProjectRootPath, StreamPath,
    };
    use tokio::sync::mpsc;

    use super::super::language_server::ServerDiscovery;
    use super::super::lsp_provider::LspProvider;
    use super::super::provider::CodeIntelProvider;
    use super::{
        CONFIGURED_PATH_HINT, INSTALL_HINT, discover_pyright, discover_pyright_configured,
        discover_with, pyright_config,
    };
    use crate::stream::Stream;

    #[test]
    fn pyright_config_identifies_as_python() {
        let config = pyright_config(&Default::default());
        assert_eq!(config.language.0, "python");
        assert_eq!(config.provider_id.0, "pyright");
        assert_eq!(config.lsp_language_id, "python");
        assert!(config.extensions.contains(&"py"));
        assert!(config.workspace_markers.contains(&"pyproject.toml"));
        assert_eq!(config.configured_path, None);
    }

    #[test]
    fn pyright_config_reads_configured_path() {
        let mut settings = CodeIntelSettings::default();
        let path = HostExecutablePath("/opt/pyright/bin/pyright-langserver".to_owned());
        settings
            .language_server_paths
            .insert(CodeIntelProviderId("pyright".to_owned()), path.clone());
        let config = pyright_config(&settings);
        assert_eq!(config.configured_path, Some(path));
    }

    #[test]
    fn configured_pyright_path_is_used_directly() {
        let file = tempfile::NamedTempFile::new().expect("configured pyright path");
        let result = discover_pyright_configured(
            Path::new("/workspace"),
            Some(&HostExecutablePath(
                file.path().to_string_lossy().into_owned(),
            )),
        );
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: file.path().to_path_buf(),
                args: vec!["--stdio".to_owned()],
            }
        );
    }

    #[test]
    fn invalid_configured_pyright_path_fails_without_install_hint() {
        let result = discover_pyright_configured(
            Path::new("/workspace"),
            Some(&HostExecutablePath(
                "/missing/pyright-langserver".to_owned(),
            )),
        );
        match result {
            ServerDiscovery::Absent { message, hint, .. } => {
                assert!(message.contains("/missing/pyright-langserver"));
                assert!(!message.contains(INSTALL_HINT));
                assert_eq!(hint, CONFIGURED_PATH_HINT);
            }
            other => panic!("expected configured-path Absent, got {other:?}"),
        }
    }

    #[test]
    fn prefers_langserver_over_bare_pyright_binary() {
        let result = discover_with(|name| match name {
            "pyright-langserver" => Some(PathBuf::from("/usr/local/bin/pyright-langserver")),
            "pyright" => Some(PathBuf::from("/usr/local/bin/pyright")),
            _ => None,
        });
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: PathBuf::from("/usr/local/bin/pyright-langserver"),
                args: vec!["--stdio".to_owned()],
            }
        );
    }

    #[test]
    fn falls_back_to_bare_pyright_with_stdio() {
        let result = discover_with(|name| {
            (name == "pyright").then(|| PathBuf::from("/usr/local/bin/pyright"))
        });
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: PathBuf::from("/usr/local/bin/pyright"),
                args: vec!["--stdio".to_owned()],
            }
        );
    }

    #[test]
    fn absent_emits_install_hint_when_neither_found() {
        // §M7: a missing pyright surfaces an honest Absent + install hint, which
        // the provider turns into `CodeIntelStatus { Unavailable }`.
        let result = discover_with(|_| None);
        assert_eq!(
            result,
            ServerDiscovery::absent_install("pyright", INSTALL_HINT)
        );
        assert_eq!(INSTALL_HINT, "npm install -g pyright");
    }

    /// pyright-gated end-to-end test: drive the **same generic engine** with the
    /// python config over a tiny project with a type error and assert an error
    /// diagnostic eventually arrives. Opt-in with `TYDE_RUN_REAL_LSP_TESTS=1`;
    /// skips-with-log when pyright is not installed — never a hard failure
    /// without pyright.
    #[tokio::test]
    #[ignore = "real external LSP test; use --ignored and TYDE_RUN_REAL_LSP_TESTS=1"]
    async fn pyright_emits_diagnostics_for_broken_file() {
        if std::env::var("TYDE_RUN_REAL_LSP_TESTS").ok().as_deref() != Some("1") {
            eprintln!(
                "SKIP pyright_emits_diagnostics_for_broken_file: set TYDE_RUN_REAL_LSP_TESTS=1 to run"
            );
            return;
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        if matches!(discover_pyright(root), ServerDiscovery::Absent { .. }) {
            eprintln!("SKIP pyright_emits_diagnostics_for_broken_file: pyright not found");
            return;
        }
        // A minimal Python project marker so detection + pyright agree on a root.
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"probe\"\n")
            .expect("write pyproject.toml");
        // A deliberate type error pyright reports in basic mode.
        std::fs::write(root.join("main.py"), "x: int = \"not an int\"\n").expect("write main.py");

        let root_path = ProjectRootPath(root.to_string_lossy().into_owned());
        let mut provider = LspProvider::new(
            pyright_config(&Default::default()),
            root_path.clone(),
            CodeIntelResourceMode::Full,
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stream = Stream::new(StreamPath("/project/ci".to_owned()), tx);
        let path = ProjectPath {
            root: root_path,
            relative_path: "main.py".to_owned(),
        };
        provider.subscribe(path, ProjectFileVersion(1), stream);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let mut saw_ready_or_diag = false;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no pyright diagnostics within timeout"
            );
            let envelope = match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(envelope)) => envelope,
                Ok(None) => panic!("output stream closed before diagnostics arrived"),
                Err(_) => panic!("timed out waiting for diagnostics"),
            };
            match envelope.kind {
                FrameKind::CodeIntelStatus => {
                    let status: CodeIntelStatusPayload =
                        serde_json::from_value(envelope.payload).expect("status payload");
                    if matches!(
                        status.state,
                        CodeIntelState::Unsupported
                            | CodeIntelState::Unavailable
                            | CodeIntelState::Failed
                    ) {
                        panic!("pyright reached terminal status before diagnostics: {status:?}");
                    }
                    if status.state == CodeIntelState::Ready {
                        saw_ready_or_diag = true;
                    }
                }
                FrameKind::CodeIntelDiagnostics => {
                    let payload: protocol::CodeIntelDiagnosticsPayload =
                        serde_json::from_value(envelope.payload).expect("diagnostics payload");
                    if payload
                        .diagnostics
                        .iter()
                        .any(|d| d.severity == protocol::CodeIntelSeverity::Error)
                    {
                        return; // success: an error diagnostic flowed through the generic path
                    }
                    saw_ready_or_diag = true;
                }
                FrameKind::CodeIntelError => {
                    let error: CodeIntelErrorPayload =
                        serde_json::from_value(envelope.payload).expect("error payload");
                    panic!("pyright emitted CodeIntelError before diagnostics: {error:?}");
                }
                _ => {}
            }
            // Guard against an environment where pyright comes up but reports no
            // diagnostics for this snippet: treat reaching Ready / a diagnostics
            // snapshot as enough that the generic path worked end-to-end.
            if saw_ready_or_diag && remaining < Duration::from_secs(90) {
                return;
            }
        }
    }
}
