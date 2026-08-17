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

use protocol::{CodeIntelLanguageId, CodeIntelProviderId};
use serde_json::json;
use settings_model::{CodeIntelSettings, HostExecutablePath};

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
