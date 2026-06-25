//! rust-analyzer discovery: **detect and hint only** (spec §2.6).
//!
//! No bundled binary and no managed download in v1 — that is deferred (§9).
//! Discovery order:
//!
//! 0. a user-configured rust-analyzer path, probed directly with no fallback.
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

use protocol::HostExecutablePath;

use super::language_server::ServerDiscovery;
use crate::process_env;

/// The install hint surfaced when rust-analyzer can't be found.
pub(crate) const INSTALL_HINT: &str = "rustup component add rust-analyzer";
pub(crate) const CUSTOM_TOOLCHAIN_HINT: &str = "This rust-analyzer is the rustup proxy for a custom toolchain; rustup cannot install the component there. Install a standalone rust-analyzer (and put it before ~/.cargo/bin on PATH), or set Tyde's rust-analyzer binary path in settings.";
const CONFIGURED_PATH_HINT: &str =
    "Set Tyde's rust-analyzer binary path to a usable rust-analyzer executable.";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_OUTPUT_LIMIT: usize = 8 * 1024;

/// Discover rust-analyzer using the real environment. This may shell out
/// (`rustup which`) and run the login-shell `PATH` probe, so callers should run
/// it off the async runtime (e.g. `spawn_blocking`).
pub(crate) fn discover_rust_analyzer(
    workspace_root: &Path,
    configured_path: Option<&HostExecutablePath>,
) -> ServerDiscovery {
    discover_with(
        configured_path.cloned(),
        process_env::find_executable_in_path,
        || rustup_which_rust_analyzer(workspace_root),
        |path| probe_rust_analyzer(path, workspace_root),
    )
}

/// Pure discovery ordering, with the two environment lookups injected so the
/// ordering/absence logic is unit-testable without a real toolchain.
fn discover_with(
    configured_path: Option<HostExecutablePath>,
    mut find_in_path: impl FnMut(&str) -> Option<PathBuf>,
    rustup_which: impl Fn() -> Option<PathBuf>,
    mut probe: impl FnMut(&Path) -> Result<(), ProbeFailure>,
) -> ServerDiscovery {
    if let Some(configured_path) = configured_path {
        let path = PathBuf::from(configured_path.0);
        return match probe(&path) {
            Ok(()) => ServerDiscovery::Found {
                binary: path,
                args: Vec::new(),
            },
            Err(failure) => configured_path_absent(failure),
        };
    }

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
    let hint = hint_for_probe_failure(&failure);
    let mut message = format!(
        "rust-analyzer at {} is not usable",
        failure.binary.display()
    );
    if hint == INSTALL_HINT {
        message.push_str(" — run `");
        message.push_str(INSTALL_HINT);
        message.push('`');
    } else {
        message.push_str(" — ");
        message.push_str(&hint);
    }
    if !failure.reason.is_empty() {
        message.push_str(": ");
        message.push_str(&failure.reason);
    }
    ServerDiscovery::Absent {
        message,
        hint,
        exit_status: failure.exit_status,
        stderr: failure.stderr,
    }
}

fn configured_path_absent(failure: ProbeFailure) -> ServerDiscovery {
    let mut message = format!(
        "configured rust-analyzer binary path {} is not usable",
        failure.binary.display()
    );
    if !failure.reason.is_empty() {
        message.push_str(": ");
        message.push_str(&failure.reason);
    }
    ServerDiscovery::Absent {
        message,
        hint: CONFIGURED_PATH_HINT.to_owned(),
        exit_status: failure.exit_status,
        stderr: failure.stderr,
    }
}

fn hint_for_probe_failure(failure: &ProbeFailure) -> String {
    if is_custom_toolchain_rustup_proxy_failure(failure.stderr.as_deref()) {
        CUSTOM_TOOLCHAIN_HINT.to_owned()
    } else {
        INSTALL_HINT.to_owned()
    }
}

fn is_custom_toolchain_rustup_proxy_failure(stderr: Option<&str>) -> bool {
    let Some(stderr) = stderr else {
        return false;
    };
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("is not installed for the custom toolchain")
        || (stderr.contains("custom toolchain")
            && (stderr.contains("cannot use") || stderr.contains("rustup component add")))
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
            None,
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
            None,
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
            None,
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
        let result = discover_with(None, |_| None, || None, ok_probe);
        assert_eq!(
            result,
            ServerDiscovery::absent_install("rust-analyzer", INSTALL_HINT)
        );
    }

    #[test]
    fn absent_includes_probe_failure_for_broken_path_candidate() {
        let result = discover_with(
            None,
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
            None,
            |binary| {
                asked = Some(binary.to_owned());
                None
            },
            || None,
            ok_probe,
        );
        assert_eq!(asked.as_deref(), Some("rust-analyzer"));
    }

    #[test]
    fn configured_path_takes_precedence_without_path_or_rustup_lookup() {
        let configured = HostExecutablePath("/opt/rust-analyzer/bin/rust-analyzer".to_owned());
        let result = discover_with(
            Some(configured),
            |_| panic!("PATH lookup must not run when rust-analyzer is configured"),
            || panic!("rustup fallback must not run when rust-analyzer is configured"),
            |path| {
                assert_eq!(path, Path::new("/opt/rust-analyzer/bin/rust-analyzer"));
                Ok(())
            },
        );
        assert_eq!(
            result,
            ServerDiscovery::Found {
                binary: PathBuf::from("/opt/rust-analyzer/bin/rust-analyzer"),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn invalid_configured_path_fails_without_fallback_or_rustup_hint() {
        let configured = HostExecutablePath("/missing/rust-analyzer".to_owned());
        let result = discover_with(
            Some(configured),
            |_| panic!("PATH lookup must not run after configured path fails"),
            || panic!("rustup fallback must not run after configured path fails"),
            |path| {
                Err(ProbeFailure {
                    binary: path.to_path_buf(),
                    reason: "failed to run probe: No such file or directory".to_owned(),
                    exit_status: None,
                    stderr: None,
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
                assert!(message.contains("configured rust-analyzer binary path"));
                assert!(message.contains("/missing/rust-analyzer"));
                assert!(!message.contains(INSTALL_HINT));
                assert_eq!(hint, CONFIGURED_PATH_HINT);
                assert_eq!(exit_status, None);
                assert_eq!(stderr, None);
            }
            other => panic!("expected configured-path Absent, got {other:?}"),
        }
    }

    #[test]
    fn custom_toolchain_proxy_failure_uses_custom_hint() {
        let result = absent(Some(ProbeFailure {
            binary: PathBuf::from("/home/u/.cargo/bin/rust-analyzer"),
            reason: "`rust-analyzer --version` failed".to_owned(),
            exit_status: Some("exit status: 1".to_owned()),
            stderr: Some(
                "error: 'rust-analyzer' is not installed for the custom toolchain 'amzn'\n\
                 help: rustup component add rust-analyzer cannot use custom toolchain"
                    .to_owned(),
            ),
        }));
        match result {
            ServerDiscovery::Absent { message, hint, .. } => {
                assert_eq!(hint, CUSTOM_TOOLCHAIN_HINT);
                assert!(message.contains("custom toolchain"));
                assert!(!message.contains("run `rustup component add rust-analyzer`"));
            }
            other => panic!("expected custom-toolchain Absent, got {other:?}"),
        }
    }

    #[test]
    fn official_toolchain_missing_component_keeps_rustup_hint() {
        let result = absent(Some(ProbeFailure {
            binary: PathBuf::from("/home/u/.cargo/bin/rust-analyzer"),
            reason: "`rust-analyzer --version` failed".to_owned(),
            exit_status: Some("exit status: 1".to_owned()),
            stderr: Some(
                "error: 'rust-analyzer' is not installed for the toolchain 'stable'\n\
                 help: run `rustup component add rust-analyzer`"
                    .to_owned(),
            ),
        }));
        match result {
            ServerDiscovery::Absent { message, hint, .. } => {
                assert_eq!(hint, INSTALL_HINT);
                assert!(message.contains(INSTALL_HINT));
            }
            other => panic!("expected official-toolchain Absent, got {other:?}"),
        }
    }
}
