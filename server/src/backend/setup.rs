use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use command_group::AsyncCommandGroup;
use protocol::{
    BackendKind, BackendSetupAction, BackendSetupCommand, BackendSetupDiagnostic,
    BackendSetupDiagnosticCode, BackendSetupInfo, BackendSetupPayload, BackendSetupStatus,
    HostPlatform,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::browse_stream::host_platform;
use crate::process_env;

pub(crate) const TYCODE_VERSION: &str = "0.9.2-pre.1";
pub(crate) const TYCODE_SETTINGS_SCHEMA_MIN_VERSION: &str = "0.10.0";
const TYCODE_RELEASE_BASE_URL: &str = "https://github.com/tigy32/Tycode/releases/download";
const TYCODE_SUBPROCESS_SHA256_AARCH64_APPLE_DARWIN: &str =
    "78e068456cd6dbdd1c0e2e4c27da4f409e7874c2c3e8d770d01c15d49341452b";
const TYCODE_SUBPROCESS_SHA256_X86_64_APPLE_DARWIN: &str =
    "46e2e7803c7e3ab91094ece81af3e894122af56971e1933f37b4d826582738a8";
const TYCODE_SUBPROCESS_SHA256_AARCH64_UNKNOWN_LINUX_MUSL: &str =
    "6e72b738dc5dbf3ec158dc8e8e8cfad2f30a0a7f615fdf64b41a3f2dc0207db2";
const TYCODE_SUBPROCESS_SHA256_X86_64_UNKNOWN_LINUX_MUSL: &str =
    "05e440903a6d44fc7d6fd74be2f748aff12f443506959725c6179379ec393dab";
const CLAUDE_CLI_CANDIDATES: &[&str] = &["claude"];
const CODEX_CLI_CANDIDATES: &[&str] = &["codex"];
const ANTIGRAVITY_CLI_CANDIDATES: &[&str] = &["agy"];
const KIRO_CLI_CANDIDATES: &[&str] = &["kiro-cli", "kiro-cli-chat"];
const HERMES_PYTHON_MODULE: &str = "tui_gateway.entry";

pub(crate) async fn collect_backend_setup() -> BackendSetupPayload {
    let platform = host_platform();
    // Probe every backend concurrently. Each probe spawns a real `<cli>
    // --version` subprocess capped at a 2s timeout, so running them
    // sequentially made host startup wait for the sum of all probes.
    let backends = futures_util::future::join_all(
        [
            BackendKind::Tycode,
            BackendKind::Kiro,
            BackendKind::Claude,
            BackendKind::Codex,
            BackendKind::Antigravity,
            BackendKind::Hermes,
        ]
        .into_iter()
        .map(|kind| probe_backend(kind, platform)),
    )
    .await;
    BackendSetupPayload { backends }
}

/// Backend setup with no real CLI probing — used by test fixtures (and any
/// host configured with `skip_real_backend_probe`) so spawning a host does
/// not pay several seconds of subprocess + network cost per fixture. Reports
/// every backend as not installed; tests drive backends through the mock
/// backend rather than the host's installed-CLI detection.
pub(crate) fn stub_backend_setup() -> BackendSetupPayload {
    let platform = host_platform();
    let backends = [
        BackendKind::Tycode,
        BackendKind::Kiro,
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Antigravity,
        BackendKind::Hermes,
    ]
    .into_iter()
    .map(|kind| {
        let install_command = install_command(kind, platform);
        let status = if install_command.is_none() {
            BackendSetupStatus::Unsupported
        } else {
            BackendSetupStatus::NotInstalled
        };
        BackendSetupInfo {
            backend_kind: kind,
            status,
            installed_version: None,
            docs_url: docs_url(kind),
            install_command,
            diagnostic: None,
            sign_in_command: sign_in_command(kind, None),
        }
    })
    .collect();
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

pub(crate) fn resolve_tycode_binary_path() -> Option<String> {
    if let Ok(path) = tycode_versioned_binary_path()
        && path.is_file()
    {
        return Some(path.to_string_lossy().to_string());
    }

    process_env::resolve_login_shell_command_path("tycode-subprocess")
        .map(|path| path.to_string_lossy().to_string())
}

pub(crate) fn tycode_probe_candidates() -> Vec<String> {
    tycode_probe_candidates_from_resolved_path(resolve_tycode_binary_path().as_deref())
}

fn tycode_probe_candidates_from_resolved_path(resolved_binary_path: Option<&str>) -> Vec<String> {
    resolved_binary_path
        .map(Path::new)
        .into_iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect()
}

async fn probe_backend(kind: BackendKind, platform: HostPlatform) -> BackendSetupInfo {
    let probe = match kind {
        BackendKind::Tycode => probe_tycode_candidates(&tycode_probe_candidates()).await,
        BackendKind::Kiro => probe_candidates(&command_candidates(KIRO_CLI_CANDIDATES)).await,
        BackendKind::Claude => probe_candidates(&command_candidates(CLAUDE_CLI_CANDIDATES)).await,
        BackendKind::Codex => probe_candidates(&command_candidates(CODEX_CLI_CANDIDATES)).await,
        BackendKind::Antigravity => probe_candidates(&antigravity_command_candidates()).await,
        BackendKind::Hermes => probe_hermes_gateway().await,
    };

    backend_setup_info_from_probe(kind, platform, probe)
}

fn backend_setup_info_from_probe(
    kind: BackendKind,
    platform: HostPlatform,
    probe: ProbeResult,
) -> BackendSetupInfo {
    let docs_url = docs_url(kind);
    let install_command = install_command(kind, platform);
    let status = backend_setup_status_for_probe(probe.status, install_command.is_some());
    let sign_in_command = sign_in_command(kind, probe.hermes_executable.as_deref());

    BackendSetupInfo {
        backend_kind: kind,
        status,
        installed_version: probe.version,
        docs_url,
        install_command,
        diagnostic: probe.diagnostic,
        sign_in_command,
    }
}

pub(crate) fn tycode_settings_schema_supported_by_pinned_release() -> bool {
    false
}

pub(crate) fn tycode_settings_schema_release_blocker_message() -> Option<String> {
    (!tycode_settings_schema_supported_by_pinned_release()).then(|| {
        format!(
            "Tyde currently pins tycode-subprocess {TYCODE_VERSION}, but Tycode's grouped \
             GetSettingsSchema settings protocol was added after that release. Tycode native \
             settings are unavailable until Tyde pins a Tycode release containing that protocol \
             (expected {TYCODE_SETTINGS_SCHEMA_MIN_VERSION} or newer)."
        )
    })
}

fn backend_setup_status_for_probe(
    probe_status: BackendSetupStatus,
    has_install_command: bool,
) -> BackendSetupStatus {
    match probe_status {
        BackendSetupStatus::Installed | BackendSetupStatus::Unavailable => probe_status,
        BackendSetupStatus::NotInstalled if !has_install_command => BackendSetupStatus::Unsupported,
        BackendSetupStatus::NotInstalled | BackendSetupStatus::Unsupported => probe_status,
    }
}

struct ProbeResult {
    status: BackendSetupStatus,
    version: Option<String>,
    diagnostic: Option<BackendSetupDiagnostic>,
    hermes_executable: Option<String>,
}

impl ProbeResult {
    fn installed(version: Option<String>) -> Self {
        Self {
            status: BackendSetupStatus::Installed,
            version,
            diagnostic: None,
            hermes_executable: None,
        }
    }

    fn not_installed() -> Self {
        Self {
            status: BackendSetupStatus::NotInstalled,
            version: None,
            diagnostic: None,
            hermes_executable: None,
        }
    }

    fn not_installed_with_diagnostic(diagnostic: BackendSetupDiagnostic) -> Self {
        Self {
            status: BackendSetupStatus::NotInstalled,
            version: None,
            diagnostic: Some(diagnostic),
            hermes_executable: None,
        }
    }

    fn unavailable(diagnostic: BackendSetupDiagnostic) -> Self {
        Self {
            status: BackendSetupStatus::Unavailable,
            version: None,
            diagnostic: Some(diagnostic),
            hermes_executable: None,
        }
    }

    fn with_hermes_executable(mut self, executable: String) -> Self {
        self.hermes_executable = Some(executable);
        self
    }
}

async fn probe_tycode_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        match validate_tycode_command(candidate).await {
            TycodeCommandValidation::Compatible { version } => {
                return ProbeResult::installed(Some(version));
            }
            TycodeCommandValidation::Incompatible { diagnostic } => {
                return ProbeResult::unavailable(diagnostic);
            }
            TycodeCommandValidation::Unavailable => continue,
        }
    }
    ProbeResult::not_installed()
}

async fn probe_candidates(candidates: &[String]) -> ProbeResult {
    for candidate in candidates {
        let Some(version) = probe_command(candidate).await else {
            continue;
        };
        return ProbeResult::installed(version);
    }
    ProbeResult::not_installed()
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
    let mut child = command.group_spawn().ok()?;
    let mut stdout_pipe = child.inner().stdout.take()?;
    let mut stderr_pipe = child.inner().stderr.take()?;

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

enum TycodeCommandValidation {
    Compatible { version: String },
    Incompatible { diagnostic: BackendSetupDiagnostic },
    Unavailable,
}

pub(crate) async fn ensure_tycode_command_compatible(command: &str) -> Result<String, String> {
    match validate_tycode_command(command).await {
        TycodeCommandValidation::Compatible { version: _ } => Ok(command.to_string()),
        TycodeCommandValidation::Incompatible { diagnostic } => Err(diagnostic.message),
        TycodeCommandValidation::Unavailable => Err(format!(
            "Failed to run tycode-subprocess --version for {command}"
        )),
    }
}

async fn validate_tycode_command(command: &str) -> TycodeCommandValidation {
    let Some(output) = run_version_command(command).await else {
        return TycodeCommandValidation::Unavailable;
    };
    let Some((stdout, stderr)) = output else {
        return TycodeCommandValidation::Incompatible {
            diagnostic: BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message: format!(
                    "Tycode command {command} did not complete a --version probe; Tyde requires tycode-subprocess {TYCODE_VERSION}"
                ),
            },
        };
    };
    let Some(version_line) = parse_tycode_version_output(&stdout, &stderr) else {
        return TycodeCommandValidation::Incompatible {
            diagnostic: BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message: format!(
                    "Tycode command {command} did not report a parseable --version; Tyde requires tycode-subprocess {TYCODE_VERSION}"
                ),
            },
        };
    };
    let Some(version) = parse_tycode_reported_version(&version_line) else {
        return TycodeCommandValidation::Incompatible {
            diagnostic: BackendSetupDiagnostic {
                code: BackendSetupDiagnosticCode::CommandFailed,
                message: format!(
                    "Tycode command {command} reported unparseable version line {version_line:?}; Tyde requires tycode-subprocess {TYCODE_VERSION}"
                ),
            },
        };
    };
    if version == TYCODE_VERSION {
        return TycodeCommandValidation::Compatible {
            version: version_line,
        };
    }
    TycodeCommandValidation::Incompatible {
        diagnostic: BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandFailed,
            message: format!(
                "Tycode command {command} reported version {version}, but Tyde requires tycode-subprocess {TYCODE_VERSION}; install the pinned Tycode release artifact"
            ),
        },
    }
}

async fn probe_explicit_hermes_python(candidate: &str) -> ProbeResult {
    match probe_hermes_python_command(candidate).await {
        Ok(version) => ProbeResult::installed(version),
        Err(err) => ProbeResult::unavailable(hermes_failure_diagnostic(
            err.explicit_override("HERMES_PYTHON"),
        )),
    }
}

async fn probe_hermes_gateway() -> ProbeResult {
    probe_hermes_gateway_with_sources(
        crate::backend::hermes::explicit_hermes_python(),
        crate::backend::hermes::explicit_hermes_executable(),
        crate::backend::hermes::hermes_executable_candidates(),
    )
    .await
}

async fn probe_hermes_gateway_with_sources(
    explicit_python: Option<String>,
    explicit_executable: Option<String>,
    executable_candidates: Vec<String>,
) -> ProbeResult {
    if let Some(candidate) = explicit_python {
        return probe_explicit_hermes_python(&candidate).await;
    }

    if let Some(candidate) = explicit_executable {
        return match crate::backend::hermes::probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                ProbeResult::installed(probe.version).with_hermes_executable(probe.executable)
            }
            Err(err) => ProbeResult::unavailable(hermes_failure_diagnostic(
                err.explicit_override("HERMES_EXECUTABLE"),
            )),
        };
    }

    let mut first_failure = None;
    for candidate in executable_candidates {
        match crate::backend::hermes::probe_hermes_cli_gateway(&candidate).await {
            Ok(probe) => {
                return ProbeResult::installed(probe.version)
                    .with_hermes_executable(probe.executable);
            }
            Err(err) => {
                tracing::debug!("Hermes executable candidate {candidate} probe failed: {err}");
                if err.code != BackendSetupDiagnosticCode::CommandNotFound || candidate != "hermes"
                {
                    first_failure.get_or_insert(err);
                }
            }
        }
    }

    let failure = crate::backend::hermes::hermes_cli_required_failure(first_failure);
    let diagnostic = hermes_failure_diagnostic(failure.clone());
    if failure.code == BackendSetupDiagnosticCode::CommandNotFound {
        ProbeResult::not_installed_with_diagnostic(diagnostic)
    } else {
        ProbeResult::unavailable(diagnostic)
    }
}

async fn probe_hermes_python_command(
    command: &str,
) -> Result<Option<String>, crate::backend::hermes::HermesProbeFailure> {
    crate::backend::hermes::probe_hermes_python_gateway_import(command)
        .await
        .map(|()| Some(format!("{command} -m {HERMES_PYTHON_MODULE}")))
}

fn hermes_failure_diagnostic(
    failure: crate::backend::hermes::HermesProbeFailure,
) -> BackendSetupDiagnostic {
    BackendSetupDiagnostic {
        code: failure.code,
        message: failure.message,
    }
}

fn antigravity_command_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(home) = home_dir() {
        let local = home.join(".local").join("bin").join("agy");
        if local.is_file() {
            candidates.push(local.to_string_lossy().to_string());
        }
    }
    for candidate in command_candidates(ANTIGRAVITY_CLI_CANDIDATES) {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
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

fn parse_tycode_reported_version(line: &str) -> Option<&str> {
    let mut parts = line.split_whitespace();
    let binary = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if binary != "tycode-subprocess" && binary != "tycode" {
        return None;
    }
    looks_like_semver(version).then_some(version)
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
        BackendKind::Antigravity => "https://antigravity.google/cli".to_string(),
        BackendKind::Hermes => {
            "https://github.com/NousResearch/hermes-agent/tree/main/ui-tui".to_string()
        }
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
        BackendKind::Antigravity => Some(BackendSetupCommand {
            title: "Install CLI".to_string(),
            description: "Install Antigravity CLI on this host.".to_string(),
            command: "curl -fsSL https://antigravity.google/cli/install.sh | bash".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Hermes => Some(BackendSetupCommand {
            title: "Install Hermes".to_string(),
            description: "Install Hermes Agent so the hermes executable is on PATH. Set HERMES_EXECUTABLE only if Tyde cannot resolve it.".to_string(),
            command: "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash".to_string(),
            display_command: None,
            runnable: true,
        }),
    }
}

fn sign_in_command(
    kind: BackendKind,
    hermes_executable: Option<&str>,
) -> Option<BackendSetupCommand> {
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
        BackendKind::Antigravity => Some(BackendSetupCommand {
            title: "Sign In".to_string(),
            description: "Start Antigravity CLI so it can prompt for login on this host."
                .to_string(),
            command: "agy".to_string(),
            display_command: None,
            runnable: true,
        }),
        BackendKind::Hermes => {
            let executable = hermes_executable?;
            Some(BackendSetupCommand {
                title: "Sign In".to_string(),
                description: "Run the Hermes setup wizard for provider authentication.".to_string(),
                command: format!("{} setup", shell_quote(executable)),
                display_command: Some(format!("{executable} setup")),
                runnable: true,
            })
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
    crate::paths::home_dir()
}

#[allow(dead_code)]
fn _tycode_release_asset_url(asset_name: &str) -> String {
    format!("{TYCODE_RELEASE_BASE_URL}/v{TYCODE_VERSION}/{asset_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        old_value: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old_value = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old_value }
        }

        fn unset(key: &'static str) -> Self {
            let old_value = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old_value }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old_value.take() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(path)
                .expect("stat executable")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod executable");
        }
    }

    fn write_fake_hermes_cli_install(dir: &Path) -> String {
        let project = dir.join("hermes-agent");
        std::fs::create_dir_all(&project).expect("create fake Hermes project");
        let python = dir.join("fake_python");
        let console = dir.join("hermes_console");
        write_executable(
            &python,
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\nexit 1\n",
        );
        write_executable(
            &console,
            &format!("#!{}\nimport sys\nsys.exit(1)\n", python.to_string_lossy()),
        );
        let hermes = dir.join("hermes");
        write_executable(
            &hermes,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'Hermes Agent v9.9.9\\nProject: {}\\n'\n  exit 0\nfi\nexec {} \"$@\"\n",
                project.to_string_lossy(),
                shell_quote(&console.to_string_lossy())
            ),
        );
        hermes.to_string_lossy().to_string()
    }

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

    #[test]
    fn tycode_version_parser_accepts_prerelease_pin() {
        let line = format!("tycode-subprocess {TYCODE_VERSION}");
        let parsed = parse_tycode_version_output(&line, "");
        assert_eq!(parsed.as_deref(), Some(line.as_str()));
        assert_eq!(
            parsed.as_deref().and_then(parse_tycode_reported_version),
            Some(TYCODE_VERSION)
        );
    }

    #[test]
    fn tycode_never_exposes_a_sign_in_command() {
        assert!(sign_in_command(BackendKind::Tycode, None).is_none());
    }

    #[test]
    fn hermes_sign_in_uses_resolved_executable_path() {
        let command = sign_in_command(BackendKind::Hermes, Some("/tmp/hermes path/hermes"))
            .expect("Hermes sign-in command");
        assert_eq!(command.command, "'/tmp/hermes path/hermes' setup");
        assert_eq!(
            command.display_command.as_deref(),
            Some("/tmp/hermes path/hermes setup")
        );
    }

    #[test]
    fn tycode_probe_candidates_use_only_resolved_absolute_paths() {
        assert_eq!(
            tycode_probe_candidates_from_resolved_path(Some("/tmp/tycode-subprocess")),
            vec!["/tmp/tycode-subprocess".to_string()]
        );
        assert!(tycode_probe_candidates_from_resolved_path(None).is_empty());
    }

    #[test]
    fn installed_tycode_probe_on_unsupported_install_platform_stays_installed() {
        let info = backend_setup_info_from_probe(
            BackendKind::Tycode,
            HostPlatform::Windows,
            ProbeResult::installed(Some(format!("tycode-subprocess {TYCODE_VERSION}"))),
        );

        assert_eq!(info.status, BackendSetupStatus::Installed);
        let expected_version = format!("tycode-subprocess {TYCODE_VERSION}");
        assert_eq!(
            info.installed_version.as_deref(),
            Some(expected_version.as_str())
        );
        assert!(info.install_command.is_none());
        assert!(info.diagnostic.is_none());
    }

    #[tokio::test]
    async fn tycode_probe_accepts_pinned_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("tycode-subprocess");
        write_executable(
            &command,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'tycode-subprocess {TYCODE_VERSION}\\n'; exit 0; fi\nexit 1\n"
            ),
        );

        let result = probe_tycode_candidates(&[command.to_string_lossy().to_string()]).await;

        assert_eq!(result.status, BackendSetupStatus::Installed);
        let expected_version = format!("tycode-subprocess {TYCODE_VERSION}");
        assert_eq!(result.version.as_deref(), Some(expected_version.as_str()));
        assert!(result.diagnostic.is_none());
    }

    #[tokio::test]
    async fn tycode_probe_rejects_mismatched_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("tycode-subprocess");
        write_executable(
            &command,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'tycode-subprocess 0.7.7\\n'; exit 0; fi\nexit 1\n",
        );

        let result = probe_tycode_candidates(&[command.to_string_lossy().to_string()]).await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        assert_eq!(result.version, None);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(diagnostic.code, BackendSetupDiagnosticCode::CommandFailed);
        assert!(
            diagnostic.message.contains("reported version 0.7.7")
                && diagnostic.message.contains(TYCODE_VERSION),
            "diagnostic should name reported and required versions: {}",
            diagnostic.message
        );
    }

    #[test]
    fn unavailable_tycode_probe_on_unsupported_install_platform_stays_unavailable() {
        let diagnostic = BackendSetupDiagnostic {
            code: BackendSetupDiagnosticCode::CommandFailed,
            message: "Tycode probe failed".to_string(),
        };
        let info = backend_setup_info_from_probe(
            BackendKind::Tycode,
            HostPlatform::Windows,
            ProbeResult::unavailable(diagnostic.clone()),
        );

        assert_eq!(info.status, BackendSetupStatus::Unavailable);
        assert!(info.install_command.is_none());
        assert_eq!(info.diagnostic, Some(diagnostic));
    }

    #[test]
    fn not_installed_tycode_without_install_support_is_unsupported() {
        let info = backend_setup_info_from_probe(
            BackendKind::Tycode,
            HostPlatform::Windows,
            ProbeResult::not_installed(),
        );

        assert_eq!(info.status, BackendSetupStatus::Unsupported);
        assert!(info.install_command.is_none());
        assert!(info.diagnostic.is_none());
    }

    #[tokio::test]
    async fn hermes_probe_does_not_mark_failed_import_installed() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("python");
        std::fs::write(&command, "#!/bin/sh\nexit 1\n").expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&command)
                .expect("stat fake python")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&command, permissions).expect("chmod fake python");
        }

        assert!(
            probe_hermes_python_command(&command.to_string_lossy())
                .await
                .is_err(),
            "failed Hermes import probes must not be treated as installed"
        );
    }

    #[tokio::test]
    async fn hermes_explicit_executable_failure_is_unavailable_without_fallback() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", "/definitely/not/hermes");

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(diagnostic.code, BackendSetupDiagnosticCode::CommandNotFound);
        assert!(
            diagnostic.message.contains("HERMES_EXECUTABLE"),
            "diagnostic should name explicit override: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_auto_cli_failure_ignores_ambient_python() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\n'; exit 0; fi\nexit 1\n",
        );
        let python = dir.path().join("python");
        write_executable(
            &python,
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\nexit 1\n",
        );
        let _python_path = EnvGuard::set("PYTHON", &python.to_string_lossy());
        let _hermes_python = EnvGuard::unset("HERMES_PYTHON");
        let _hermes_executable = EnvGuard::unset("HERMES_EXECUTABLE");

        let result = probe_hermes_gateway_with_sources(
            None,
            None,
            vec![hermes.to_string_lossy().to_string()],
        )
        .await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        assert_eq!(result.version, None);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingProjectRoot
        );
        assert!(
            diagnostic.message.contains("Found Hermes CLI"),
            "diagnostic should preserve CLI failure instead of using ambient Python: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_cli_wrapper_without_project_venv_is_installed() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let hermes = write_fake_hermes_cli_install(dir.path());
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::unset("HERMES_EXECUTABLE");

        let result = probe_hermes_gateway_with_sources(None, None, vec![hermes.clone()]).await;

        assert_eq!(result.status, BackendSetupStatus::Installed);
        assert_eq!(result.version.as_deref(), Some("Hermes Agent v9.9.9"));
        assert!(result.diagnostic.is_none());
        assert_eq!(result.hermes_executable.as_deref(), Some(hermes.as_str()));
    }

    #[tokio::test]
    async fn hermes_found_unusable_cli_is_unavailable_not_not_installed() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let project = dir.path().join("hermes-agent");
        std::fs::create_dir_all(&project).expect("create project without venv");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\nProject: {}\\n'; exit 0; fi\nexit 1\n",
                project.to_string_lossy()
            ),
        );
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::unset("HERMES_EXECUTABLE");

        let result = probe_hermes_gateway_with_sources(
            None,
            None,
            vec![hermes.to_string_lossy().to_string()],
        )
        .await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingGatewayPython
        );
        assert!(
            diagnostic.message.contains("Hermes Agent v9.9.9")
                && diagnostic
                    .message
                    .contains(&project.to_string_lossy().to_string()),
            "diagnostic should name version and project: {}",
            diagnostic.message
        );
        assert!(
            !diagnostic.message.contains("so `hermes` is on PATH")
                && !diagnostic.message.contains("set HERMES_EXECUTABLE"),
            "found-unusable diagnostic should not recommend PATH/HERMES_EXECUTABLE remedies: {}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("Re-run the Hermes installer")
                && diagnostic.message.contains("HERMES_PYTHON"),
            "found-unusable diagnostic should include an actionable gateway-Python remedy: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_missing_project_root_is_typed_diagnostic() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\n'; exit 0; fi\nexit 1\n",
        );
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", &hermes.to_string_lossy());

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingProjectRoot
        );
        assert!(
            diagnostic.message.contains("Project:"),
            "diagnostic should describe missing Project line: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_missing_gateway_python_is_typed_diagnostic() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let project = dir.path().join("hermes-agent");
        std::fs::create_dir_all(&project).expect("create project without venv");
        let hermes = dir.path().join("hermes");
        write_executable(
            &hermes,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'Hermes Agent v9.9.9\\nProject: {}\\n'; exit 0; fi\nexit 1\n",
                project.to_string_lossy()
            ),
        );
        let _python = EnvGuard::unset("HERMES_PYTHON");
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", &hermes.to_string_lossy());

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::MissingGatewayPython
        );
        assert!(
            diagnostic
                .message
                .contains("could not resolve a Python interpreter"),
            "diagnostic should mention the unresolved gateway Python: {}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains("Re-run the Hermes installer")
                && diagnostic.message.contains("HERMES_PYTHON"),
            "diagnostic should include an actionable gateway-Python remedy: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_python_override_failure_is_authoritative() {
        let _lock = crate::backend::hermes::TEST_HERMES_OVERRIDE_LOCK
            .lock()
            .await;
        let dir = tempfile::tempdir().expect("create tempdir");
        let fake_python = dir.path().join("python");
        write_executable(&fake_python, "#!/bin/sh\nexit 1\n");
        let valid_hermes = write_fake_hermes_cli_install(dir.path());
        let _python = EnvGuard::set("HERMES_PYTHON", &fake_python.to_string_lossy());
        let _executable = EnvGuard::set("HERMES_EXECUTABLE", &valid_hermes);

        let result = probe_hermes_gateway().await;

        assert_eq!(result.status, BackendSetupStatus::Unavailable);
        let diagnostic = result.diagnostic.expect("diagnostic");
        assert_eq!(
            diagnostic.code,
            BackendSetupDiagnosticCode::GatewayImportFailed
        );
        assert!(
            diagnostic.message.contains("HERMES_PYTHON"),
            "diagnostic should name explicit Python override: {}",
            diagnostic.message
        );
    }

    #[tokio::test]
    async fn hermes_probe_timeout_is_not_installed_without_version() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let command = dir.path().join("python");
        std::fs::write(&command, "#!/bin/sh\nsleep 5\n").expect("write fake python");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&command)
                .expect("stat fake python")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&command, permissions).expect("chmod fake python");
        }

        assert!(
            probe_hermes_python_command(&command.to_string_lossy())
                .await
                .is_err(),
            "timed-out Hermes import probes must not be treated as installed"
        );
    }
}
