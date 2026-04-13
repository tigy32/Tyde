use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::backends::tycode::{install_local_tycode_subprocess, resolve_tycode_subprocess_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendDepResult {
    pub available: bool,
    pub binary_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendDependencyStatus {
    pub tycode: BackendDepResult,
    pub codex: BackendDepResult,
    pub claude: BackendDepResult,
    pub kiro: BackendDepResult,
    pub gemini: BackendDepResult,
}

fn check_binary(binary: &str) -> BackendDepResult {
    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };

    let available = std::process::Command::new(which_cmd)
        .arg(binary)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    BackendDepResult {
        available,
        binary_name: binary.to_string(),
    }
}

pub fn check_backend_dependencies() -> BackendDependencyStatus {
    BackendDependencyStatus {
        tycode: BackendDepResult {
            available: resolve_tycode_subprocess_path().is_ok(),
            binary_name: "tycode-subprocess".to_string(),
        },
        codex: check_binary("codex"),
        claude: check_binary("claude"),
        kiro: check_binary("kiro-cli"),
        gemini: check_binary("gemini"),
    }
}

async fn install_codex() -> Result<(), String> {
    let output = Command::new("npm")
        .args(["install", "-g", "@openai/codex"])
        .output()
        .await
        .map_err(|err| format!("Failed to run npm: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Failed to install codex: {stderr}"))
}

async fn install_claude_code() -> Result<(), String> {
    let output = Command::new("sh")
        .args(["-c", "curl -fsSL https://claude.ai/install.sh | bash"])
        .output()
        .await
        .map_err(|err| format!("Failed to run install command: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Failed to install claude-code: {stderr}"))
}

async fn install_kiro() -> Result<(), String> {
    let output = Command::new("sh")
        .args([
            "-c",
            "curl -fsSL https://cli.kiro.dev/install | bash -s -- --force",
        ])
        .output()
        .await
        .map_err(|err| format!("Failed to run install script: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Failed to install kiro: {stderr}"))
}

async fn install_gemini() -> Result<(), String> {
    let output = Command::new("npm")
        .args(["install", "-g", "@google/gemini-cli"])
        .output()
        .await
        .map_err(|err| format!("Failed to run npm: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Failed to install gemini-cli: {stderr}"))
}

pub async fn install_backend_dependency(backend_kind: &str) -> Result<(), String> {
    match backend_kind {
        "tycode" => install_local_tycode_subprocess().await,
        "codex" => install_codex().await,
        "claude" => install_claude_code().await,
        "kiro" => install_kiro().await,
        "gemini" => install_gemini().await,
        other => Err(format!("Unknown backend kind: {other}")),
    }
}
