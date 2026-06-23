//! rust-analyzer discovery: **detect and hint only** (spec §2.6).
//!
//! No bundled binary and no managed download in v1 — that is deferred (§9).
//! Discovery order:
//!
//! 1. `find_executable_in_path("rust-analyzer")` (reuses the login-shell `PATH`
//!    resolution in `process_env`, so `~/.cargo/bin` is covered).
//! 2. else `rustup which --toolchain stable rust-analyzer`.
//! 3. else [`ServerDiscovery::Absent`] carrying the install hint
//!    `rustup component add rust-analyzer`.
//!
//! The result is the shared [`ServerDiscovery`] the generic provider consumes
//! for every language; pyright's analogous discovery lives in
//! [`super::pyright`]. No `window.confirm` / native dialog is involved —
//! discovery is silent and absence surfaces as a typed
//! `CodeIntelStatus { Unavailable }`.

use std::path::PathBuf;
use std::process::Command;

use super::language_server::ServerDiscovery;
use crate::process_env;

/// The install hint surfaced when rust-analyzer can't be found.
pub(crate) const INSTALL_HINT: &str = "rustup component add rust-analyzer";

/// Discover rust-analyzer using the real environment. This may shell out
/// (`rustup which`) and run the login-shell `PATH` probe, so callers should run
/// it off the async runtime (e.g. `spawn_blocking`).
pub(crate) fn discover_rust_analyzer() -> ServerDiscovery {
    discover_with(
        process_env::find_executable_in_path,
        rustup_which_rust_analyzer,
    )
}

/// Pure discovery ordering, with the two environment lookups injected so the
/// ordering/absence logic is unit-testable without a real toolchain.
fn discover_with(
    mut find_in_path: impl FnMut(&str) -> Option<PathBuf>,
    rustup_which: impl Fn() -> Option<PathBuf>,
) -> ServerDiscovery {
    if let Some(path) = find_in_path("rust-analyzer") {
        return ServerDiscovery::Found {
            binary: path,
            args: Vec::new(),
        };
    }
    if let Some(path) = rustup_which() {
        return ServerDiscovery::Found {
            binary: path,
            args: Vec::new(),
        };
    }
    ServerDiscovery::Absent {
        hint: INSTALL_HINT.to_owned(),
    }
}

/// `rustup which --toolchain stable rust-analyzer`, returning the resolved path
/// if rustup is installed and the component is present.
fn rustup_which_rust_analyzer() -> Option<PathBuf> {
    let rustup = process_env::find_executable_in_path("rustup")?;
    let output = Command::new(rustup)
        .args(["which", "--toolchain", "stable", "rust-analyzer"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        return None;
    }
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_path_over_rustup() {
        let result = discover_with(
            |_| Some(PathBuf::from("/usr/local/bin/rust-analyzer")),
            || Some(PathBuf::from("/should/not/be/used")),
        );
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: PathBuf::from("/usr/local/bin/rust-analyzer"),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn falls_back_to_rustup_when_not_on_path() {
        let result = discover_with(
            |_| None,
            || Some(PathBuf::from("/home/u/.rustup/.../rust-analyzer")),
        );
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: PathBuf::from("/home/u/.rustup/.../rust-analyzer"),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn absent_with_install_hint_when_neither_found() {
        let result = discover_with(|_| None, || None);
        assert_eq!(
            result,
            ServerDiscovery::Absent {
                hint: INSTALL_HINT.to_owned(),
            }
        );
    }

    #[test]
    fn looks_up_the_right_binary_name() {
        // Guards against a typo'd binary name in the PATH lookup.
        let mut asked = None;
        let _ = discover_with(
            |binary| {
                asked = Some(binary.to_owned());
                None
            },
            || None,
        );
        assert_eq!(asked.as_deref(), Some("rust-analyzer"));
    }
}
