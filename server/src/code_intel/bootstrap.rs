//! rust-analyzer discovery: **detect and hint only** (spec §2.6).
//!
//! No bundled binary and no managed download in v1 — that is deferred (§9).
//! Discovery order:
//!
//! 1. `find_executable_in_path("rust-analyzer")` (reuses the login-shell `PATH`
//!    resolution in `process_env`, so `~/.cargo/bin` is covered).
//! 2. else `rustup which rust-analyzer` in the workspace root.
//! 3. else [`ServerDiscovery::Absent`] carrying the install hint
//!    `rustup component add rust-analyzer`.
//!
//! The result is the shared [`ServerDiscovery`] the generic provider consumes
//! for every language; pyright's analogous discovery lives in
//! [`super::pyright`]. No `window.confirm` / native dialog is involved —
//! discovery is silent and absence surfaces as a typed
//! `CodeIntelStatus { Unavailable }`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::language_server::ServerDiscovery;
use crate::process_env;

/// The install hint surfaced when rust-analyzer can't be found.
pub(crate) const INSTALL_HINT: &str = "rustup component add rust-analyzer";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_OUTPUT_LIMIT: usize = 8 * 1024;

/// Discover rust-analyzer using the real environment. This may shell out
/// (`rustup which`) and run the login-shell `PATH` probe, so callers should run
/// it off the async runtime (e.g. `spawn_blocking`).
pub(crate) fn discover_rust_analyzer(workspace_root: &Path) -> ServerDiscovery {
    discover_with(
        process_env::find_executable_in_path,
        || rustup_which_rust_analyzer(workspace_root),
        |path| probe_rust_analyzer(path, workspace_root),
    )
}

/// Pure discovery ordering, with the two environment lookups injected so the
/// ordering/absence logic is unit-testable without a real toolchain.
fn discover_with(
    mut find_in_path: impl FnMut(&str) -> Option<PathBuf>,
    rustup_which: impl Fn() -> Option<PathBuf>,
    mut probe: impl FnMut(&Path) -> Result<(), ProbeFailure>,
) -> ServerDiscovery {
    let mut first_failure = None;
    if let Some(path) = find_in_path("rust-analyzer") {
        match probe(&path) {
            Ok(()) => {
                return ServerDiscovery::Found {
                    binary: path,
                    args: Vec::new(),
                };
            }
            Err(failure) => first_failure = Some(failure),
        }
    }
    if let Some(path) = rustup_which() {
        match probe(&path) {
            Ok(()) => {
                return ServerDiscovery::Found {
                    binary: path,
                    args: Vec::new(),
                };
            }
            Err(failure) => {
                if first_failure.is_none() {
                    first_failure = Some(failure);
                }
            }
        }
    }
    absent(first_failure)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeFailure {
    binary: PathBuf,
    reason: String,
    exit_status: Option<String>,
    stderr: Option<String>,
}

fn absent(failure: Option<ProbeFailure>) -> ServerDiscovery {
    let Some(failure) = failure else {
        return ServerDiscovery::absent_install("rust-analyzer", INSTALL_HINT);
    };
    let mut message = format!(
        "rust-analyzer at {} is not usable — run `{INSTALL_HINT}`",
        failure.binary.display()
    );
    if !failure.reason.is_empty() {
        message.push_str(": ");
        message.push_str(&failure.reason);
    }
    ServerDiscovery::Absent {
        message,
        hint: INSTALL_HINT.to_owned(),
        exit_status: failure.exit_status,
        stderr: failure.stderr,
    }
}

fn probe_rust_analyzer(path: &Path, workspace_root: &Path) -> Result<(), ProbeFailure> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_child_path(&mut command);
    match run_bounded(command, PROBE_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(ProbeFailure {
            binary: path.to_path_buf(),
            reason: "`rust-analyzer --version` failed".to_owned(),
            exit_status: Some(output.status.to_string()),
            stderr: trimmed_output(&output.stderr),
        }),
        Err(reason) => Err(ProbeFailure {
            binary: path.to_path_buf(),
            reason,
            exit_status: None,
            stderr: None,
        }),
    }
}

/// `rustup which rust-analyzer` in the workspace root, returning the resolved
/// path if rustup is installed and the component is present. Avoid
/// `--toolchain stable` so rustup respects a workspace override/default
/// toolchain.
fn rustup_which_rust_analyzer(workspace_root: &Path) -> Option<PathBuf> {
    let rustup = process_env::find_executable_in_path("rustup")?;
    let mut command = Command::new(rustup);
    command
        .args(["which", "rust-analyzer"])
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_child_path(&mut command);
    let output = run_bounded(command, PROBE_TIMEOUT).ok()?;
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

fn apply_child_path(command: &mut Command) {
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
}

fn run_bounded(mut command: Command, timeout: Duration) -> Result<std::process::Output, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run probe: {error}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("failed to read probe output: {error}"));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("probe timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("failed waiting for probe: {error}")),
        }
    }
}

fn trimmed_output(bytes: &[u8]) -> Option<String> {
    let output = String::from_utf8_lossy(bytes);
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(limit_tail(trimmed, PROBE_OUTPUT_LIMIT))
}

fn limit_tail(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &value[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_probe(_: &Path) -> Result<(), ProbeFailure> {
        Ok(())
    }

    #[test]
    fn prefers_path_over_rustup() {
        let result = discover_with(
            |_| Some(PathBuf::from("/usr/local/bin/rust-analyzer")),
            || Some(PathBuf::from("/should/not/be/used")),
            ok_probe,
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
            ok_probe,
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
    fn rejects_broken_path_candidate_and_uses_rustup_candidate() {
        let result = discover_with(
            |_| Some(PathBuf::from("/home/u/.cargo/bin/rust-analyzer")),
            || {
                Some(PathBuf::from(
                    "/home/u/.rustup/toolchains/stable/bin/rust-analyzer",
                ))
            },
            |path| {
                if path == Path::new("/home/u/.cargo/bin/rust-analyzer") {
                    Err(ProbeFailure {
                        binary: path.to_path_buf(),
                        reason: "`rust-analyzer --version` failed".to_owned(),
                        exit_status: Some("exit status: 1".to_owned()),
                        stderr: Some("error: Unknown binary 'rust-analyzer'".to_owned()),
                    })
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: PathBuf::from("/home/u/.rustup/toolchains/stable/bin/rust-analyzer"),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn absent_with_install_hint_when_neither_found() {
        let result = discover_with(|_| None, || None, ok_probe);
        assert_eq!(
            result,
            ServerDiscovery::absent_install("rust-analyzer", INSTALL_HINT)
        );
    }

    #[test]
    fn absent_includes_probe_failure_for_broken_path_candidate() {
        let result = discover_with(
            |_| Some(PathBuf::from("/home/u/.cargo/bin/rust-analyzer")),
            || None,
            |path| {
                Err(ProbeFailure {
                    binary: path.to_path_buf(),
                    reason: "`rust-analyzer --version` failed".to_owned(),
                    exit_status: Some("exit status: 1".to_owned()),
                    stderr: Some("error: Unknown binary 'rust-analyzer'".to_owned()),
                })
            },
        );
        match result {
            ServerDiscovery::Absent {
                message,
                hint,
                exit_status,
                stderr,
            } => {
                assert!(message.contains("/home/u/.cargo/bin/rust-analyzer"));
                assert!(message.contains(INSTALL_HINT));
                assert_eq!(hint, INSTALL_HINT);
                assert_eq!(exit_status.as_deref(), Some("exit status: 1"));
                assert_eq!(
                    stderr.as_deref(),
                    Some("error: Unknown binary 'rust-analyzer'")
                );
            }
            other => panic!("expected Absent with probe details, got {other:?}"),
        }
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
            ok_probe,
        );
        assert_eq!(asked.as_deref(), Some("rust-analyzer"));
    }
}
