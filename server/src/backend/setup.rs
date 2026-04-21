use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use protocol::{
    BackendKind, BackendSetupAction, BackendSetupCommand, BackendSetupInfo, BackendSetupPayload,
    BackendSetupStatus, HostPlatform,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::backend::codex::discover_models;
use crate::browse_stream::host_platform;
use crate::process_env;

pub(crate) const TYCODE_VERSION: &str = "0.7.3";
const TYCODE_RELEASE_BASE_URL: &str = "https://github.com/tigy32/Tycode/releases/download";
const TYCODE_SUBPROCESS_SHA256_AARCH64_APPLE_DARWIN: &str =
    "7fa6927b0bc6f6a7aa9b9577b0b69bcb19fa3c074dbebfc2f4d8143cfa080aef";
const TYCODE_SUBPROCESS_SHA256_X86_64_APPLE_DARWIN: &str =
    "c1ce63b47604c18af8dc3b87fad1865612802e4e92402bf0a375ce6205e46baf";
const TYCODE_SUBPROCESS_SHA256_AARCH64_UNKNOWN_LINUX_MUSL: &str =
    "4d776bdaa8388103135021805c0be8954a2aca88f2b357bc75647a32a1c5d9c3";
const TYCODE_SUBPROCESS_SHA256_X86_64_UNKNOWN_LINUX_MUSL: &str =
    "809e47d50f09dd0885b577e091bf2efc44850ed88a2dfdc457b1e3423759c94f";
const CLAUDE_CLI_CANDIDATES: &[&str] = &["claude"];
const CODEX_CLI_CANDIDATES: &[&str] = &["codex"];
const GEMINI_CLI_CANDIDATES: &[&str] = &["gemini"];
const KIRO_CLI_CANDIDATES: &[&str] = &["kiro-cli", "kiro-cli-chat"];

pub(crate) async fn collect_backend_setup() -> BackendSetupPayload {
    let platform = host_platform();
    let mut backends = Vec::new();
    for kind in [
        BackendKind::Tycode,
        BackendKind::Kiro,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Gemini,
    ] {
        backends.push(probe_backend(kind, platform).await);
    }
    BackendSetupPayload { backends }
}

pub(crate) async fn runnable_command(
    backend_kind: BackendKind,
    action: BackendSetupAction,
) -> Option<String> {
    let payload = collect_backend_setup().await;
    let info = payload
        .backends
        .into_iter()
        .find(|info| info.backend_kind == backend_kind)?;

    let command = match action {
        BackendSetupAction::Install => info.install_command,
        BackendSetupAction::SignIn => info.sign_in_command,
    };

    command
        .as_ref()
        .filter(|cmd| cmd.runnable)
        .map(|cmd| cmd.command.clone())
}

pub(crate) fn tycode_versioned_binary_path() -> Result<PathBuf, String> {
    Ok(home_dir()?
        .join(".tyde")
        .join("tycode")
        .join(TYCODE_VERSION)
        .join("tycode-subprocess"))
}

pub(crate) fn tycode_probe_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(path) = tycode_versioned_binary_path() {
        candidates.push(path.to_string_lossy().to_string());
    }
    candidates.push("tycode-subprocess".to_string());
    candidates
}

async fn probe_backend(kind: BackendKind, platform: HostPlatform) -> BackendSetupInfo {
    let docs_url = docs_url(kind);
    let install_command = install_command(kind, platform);
    let sign_in_command = sign_in_command(kind);

    let probe = match kind {
        BackendKind::Tycode => probe_tycode_candidates(&tycode_probe_candidates()).await,
        BackendKind::Kiro => probe_candidates(&command_candidates(KIRO_CLI_CANDIDATES)).await,
        BackendKind::Claude => probe_candidates(&command_candidates(CLAUDE_CLI_CANDIDATES)).await,
        BackendKind::Codex => probe_candidates(&command_candidates(CODEX_CLI_CANDIDATES)).await,
        BackendKind::Gemini => probe_candidates(&command_candidates(GEMINI_CLI_CANDIDATES)).await,
    };

    if kind == BackendKind::Codex && probe.installed {
        discover_models().await;
    }

    let status = if install_command.is_none() {
        BackendSetupStatus::Unsupported
    } else if probe.installed {
        BackendSetupStatus::Installed
    } else {
        BackendSetupStatus::NotInstalled
    };

    BackendSetupInfo {
        backend_kind: kind,
        status,
        installed_version: probe.version,
        docs_url,
        install_command,
        sign_in_command,
    }
}

struct ProbeResult {
    installed: bool,
    version: Option<String>,
}

async fn probe_tycode_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        let Some(version) = probe_tycode_command(candidate).await else {
            continue;
        };
        return ProbeResult {
            installed: true,
            version,
        };
    }
    ProbeResult {
        installed: false,
        version: None,
    }
}

async fn probe_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        let Some(version) = probe_command(candidate).await else {
            continue;
        };
        return ProbeResult {
            installed: true,
            version,
        };
    }
    ProbeResult {
        installed: false,
        version: None,
    }
}

async fn probe_tycode_command(command: &str) -> Option<Option<String>> {
    let output = run_version_command(command).await?;
    let Some((stdout, stderr)) = output else {
        return Some(None);
    };
    Some(parse_tycode_version_output(&stdout, &stderr))
}

async fn probe_command(command: &str) -> Option<Option<String>> {
    let output = run_version_command(command).await?;
    let Some((stdout, stderr)) = output else {
        return Some(None);
    };
    let version = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string());
    Some(version)
}

async fn run_version_command(command: &str) -> Option<Option<(String, String)>> {
    let mut command = Command::new(command);
    command
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    let mut child = command.spawn().ok()?;
    let mut stdout_pipe = child.stdout.take()?;
    let mut stderr_pipe = child.stderr.take()?;

    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => return Some(None),
        Err(_) => {
            let _ = child.kill().await;
            return Some(None);
        }
    };

    let mut stdout_bytes = Vec::new();
    if stdout_pipe.read_to_end(&mut stdout_bytes).await.is_err() {
        return Some(None);
    }
    let mut stderr_bytes = Vec::new();
    if stderr_pipe.read_to_end(&mut stderr_bytes).await.is_err() {
        return Some(None);
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    let _ = status;
    Some(Some((stdout, stderr)))
}

fn command_candidates(defaults: &[&str]) -> Vec<String> {
    let mut candidates = Vec::<String>::new();
    for default in defaults {
        if let Some(path) = process_env::find_executable_in_path(default) {
            let path = path.to_string_lossy().to_string();
            if !candidates.contains(&path) {
                candidates.push(path);
            }
        }

        let candidate = default.to_string();
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn parse_tycode_version_output(stdout: &str, stderr: &str) -> Option<String> {
    for line in stdout.lines().chain(stderr.lines()).map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(version) = parse_tycode_plain_text_version_line(line) {
            return Some(version);
        }
        if let Some(version) = parse_tycode_version_frame(line) {
            return Some(version);
        }
    }
    None
}

fn parse_tycode_plain_text_version_line(line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    let binary = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if binary != "tycode-subprocess" && binary != "tycode" {
        return None;
    }
    if !looks_like_semver(version) {
        return None;
    }
    Some(line.to_string())
}

fn parse_tycode_version_frame(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = value.get("kind").and_then(serde_json::Value::as_str)?;
    if !kind.eq_ignore_ascii_case("version") {
        return None;
    }
    let version = value
        .get("version")
        .or_else(|| value.get("data").and_then(|data| data.get("version")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())?;
    let binary = value
        .get("binary")
        .or_else(|| value.get("data").and_then(|data| data.get("binary")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .unwrap_or("tycode-subprocess");
    Some(format!("{binary} {version}"))
}

fn looks_like_semver(value: &str) -> bool {
    let mut saw_digit = false;
    let mut saw_dot = false;
    for byte in value.bytes() {
        match byte {
            b'0'..=b'9' => saw_digit = true,
            b'.' => saw_dot = true,
            b'-' | b'+' | b'a'..=b'z' | b'A'..=b'Z' => {}
            _ => return false,
        }
    }
    saw_digit && saw_dot
}

fn docs_url(kind: BackendKind) -> String {
    match kind {
        BackendKind::Tycode => {
            format!("https://github.com/tigy32/Tycode/releases/tag/v{TYCODE_VERSION}")
        }
        BackendKind::Kiro => "https://kiro.dev/docs/cli/installation/".to_string(),
        BackendKind::Claude => {
            "https://docs.anthropic.com/en/docs/claude-code/getting-started".to_string()
        }
        BackendKind::Codex => "https://help.openai.com/en/articles/11096431".to_string(),
        BackendKind::Gemini => "https://github.com/google-gemini/gemini-cli".to_string(),
    }
}

fn install_command(kind: BackendKind, platform: HostPlatform) -> Option<BackendSetupCommand> {
    match kind {
        BackendKind::Tycode => tycode_install_command(platform),
        BackendKind::Kiro => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description: "Install Kiro CLI on this host. Kiro opens a browser for authentication after install.".to_string(),
            command: match platform {
                HostPlatform::Windows => {
                    "powershell -ExecutionPolicy Bypass -Command \"irm 'https://cli.kiro.dev/install.ps1' | iex\"".to_string()
                }
                _ => "curl -fsSL https://cli.kiro.dev/install | bash".to_string(),
            },
            display_command: None,
            runnable: true,
        }),
        BackendKind::Claude => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description:
                "Install Claude Code with npm. Anthropic documents Node.js 18+ as a prerequisite."
                    .to_string(),
            command: "npm install -g @anthropic-ai/claude-code".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Codex => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description: "Install Codex CLI with npm.".to_string(),
            command: "npm install -g @openai/codex".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Gemini => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description:
                "Install Gemini CLI with npm. Google documents Node.js 20+ as a prerequisite."
                    .to_string(),
            command: "npm install -g @google/gemini-cli".to_string(),
            display_command: None,
            runnable: true,
        }),
    }
}

fn sign_in_command(kind: BackendKind) -> Option<BackendSetupCommand> {
    match kind {
        BackendKind::Tycode => None,
        BackendKind::Kiro => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start the Kiro login flow for this host.".to_string(),
            command: "kiro-cli login".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Claude => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start Claude Code so it can prompt for login on this host.".to_string(),
            command: "claude".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Codex => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start the Codex login flow for this host.".to_string(),
            command: "codex --login".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Gemini => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start Gemini CLI so it can prompt for login on this host.".to_string(),
            command: "gemini".to_string(),
            display_command: None,
            runnable: true,
        }),
    }
}

fn tycode_install_command(platform: HostPlatform) -> Option<BackendSetupCommand> {
    match platform {
        HostPlatform::Macos | HostPlatform::Linux => Some(BackendSetupCommand {
            title: "Install release artifact".to_string(),
            description: format!(
                "Download the Tycode v{TYCODE_VERSION} release artifact for this host, extract tycode-subprocess, and install it into ~/.tyde/tycode/{TYCODE_VERSION}."
            ),
            command: tycode_unix_install_command(),
            display_command: Some(format!(
                "sh -lc 'download Tycode v{TYCODE_VERSION} into ~/.tyde/tycode/{TYCODE_VERSION}'"
            )),
            runnable: true,
        }),
        HostPlatform::Windows | HostPlatform::Other => None,
    }
}

fn tycode_unix_install_command() -> String {
    format!(
        r#"set -euo pipefail

VERSION="{version}"
BASE_URL="{release_base}/v{version}"
HOME_DIR="${{HOME:-}}"
[ -n "$HOME_DIR" ] || {{ echo "HOME is empty" >&2; exit 1; }}
command -v python3 >/dev/null 2>&1 || {{ echo "python3 is required for Tycode install" >&2; exit 1; }}
OS="$(uname -s)"
ARCH="$(uname -m)"

sha256_file() {{
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{{print $1}}'
    return
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{{print $1}}'
    return
  fi
  echo "No SHA256 tool found" >&2
  exit 1
}}

fsync_path() {{
  python3 - "$1" <<'PY'
import os
import sys

path = sys.argv[1]
fd = os.open(path, os.O_RDONLY)
try:
    os.fsync(fd)
finally:
    os.close(fd)
PY
}}

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64)
        ASSET="tycode-subprocess-aarch64-apple-darwin.tar.xz"
        EXPECTED_SHA256="{sha_macos_arm64}"
        ;;
      x86_64|amd64)
        ASSET="tycode-subprocess-x86_64-apple-darwin.tar.xz"
        EXPECTED_SHA256="{sha_macos_x64}"
        ;;
      *) echo "Unsupported Tycode architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      arm64|aarch64)
        ASSET="tycode-subprocess-aarch64-unknown-linux-musl.tar.xz"
        EXPECTED_SHA256="{sha_linux_arm64}"
        ;;
      x86_64|amd64)
        ASSET="tycode-subprocess-x86_64-unknown-linux-musl.tar.xz"
        EXPECTED_SHA256="{sha_linux_x64}"
        ;;
      *) echo "Unsupported Tycode architecture: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported Tycode OS: $OS" >&2
    exit 1
    ;;
esac

URL="${{BASE_URL}}/${{ASSET}}"
INSTALL_ROOT="${{HOME_DIR}}/.tyde/tycode"
DEST_DIR="${{INSTALL_ROOT}}/${{VERSION}}"
TMP_ROOT="$(mktemp -d)"
ARCHIVE="${{TMP_ROOT}}/${{ASSET}}"
STAGED_BINARY="${{DEST_DIR}}/tycode-subprocess.tmp.$$"
FINAL_BINARY="${{DEST_DIR}}/tycode-subprocess"
cleanup() {{
  rm -rf "$TMP_ROOT"
  rm -f "$STAGED_BINARY"
}}
trap cleanup EXIT

mkdir -p "$DEST_DIR"
curl -fL "$URL" -o "$ARCHIVE"
ACTUAL_SHA256="$(sha256_file "$ARCHIVE")"
[ "$ACTUAL_SHA256" = "$EXPECTED_SHA256" ] || {{
  echo "Tycode SHA256 mismatch for $ASSET: expected $EXPECTED_SHA256 got $ACTUAL_SHA256" >&2
  exit 1
}}
tar -xJf "$ARCHIVE" -C "$TMP_ROOT"
BINARY="$(find "$TMP_ROOT" -type f -name 'tycode-subprocess' | head -n 1)"
[ -n "$BINARY" ] || {{ echo "Downloaded Tycode asset did not contain tycode-subprocess" >&2; exit 1; }}
install -m 755 "$BINARY" "$STAGED_BINARY"
fsync_path "$STAGED_BINARY"
mv -f "$STAGED_BINARY" "$FINAL_BINARY"
fsync_path "$DEST_DIR"
"$FINAL_BINARY" --version
"#,
        version = TYCODE_VERSION,
        release_base = TYCODE_RELEASE_BASE_URL,
        sha_macos_arm64 = TYCODE_SUBPROCESS_SHA256_AARCH64_APPLE_DARWIN,
        sha_macos_x64 = TYCODE_SUBPROCESS_SHA256_X86_64_APPLE_DARWIN,
        sha_linux_arm64 = TYCODE_SUBPROCESS_SHA256_AARCH64_UNKNOWN_LINUX_MUSL,
        sha_linux_x64 = TYCODE_SUBPROCESS_SHA256_X86_64_UNKNOWN_LINUX_MUSL,
    )
}

fn home_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory".to_string())?;
    let trimmed = home.trim();
    if trimmed.is_empty() {
        return Err("HOME is empty".to_string());
    }
    Ok(PathBuf::from(trimmed))
}

#[allow(dead_code)]
fn _tycode_release_asset_url(asset_name: &str) -> String {
    format!("{TYCODE_RELEASE_BASE_URL}/v{TYCODE_VERSION}/{asset_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tycode_version_parser_ignores_task_update_frames() {
        let parsed =
            parse_tycode_version_output(r#"{"kind":"TaskUpdate","data":{"tasks":[]}}"#, "");
        assert_eq!(parsed, None);
    }

    #[test]
    fn tycode_version_parser_reads_plain_text_version_after_json_frame() {
        let parsed = parse_tycode_version_output(
            "{\"kind\":\"TaskUpdate\",\"data\":{\"tasks\":[]}}\ntycode-subprocess 0.7.3",
            "",
        );
        assert_eq!(parsed.as_deref(), Some("tycode-subprocess 0.7.3"));
    }
}
