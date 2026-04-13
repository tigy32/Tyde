use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, Mutex};
use tyde_protocol::protocol::{ChatEvent, SubprocessExitData};

use crate::agent::CommandExecutor;
use crate::backends::transport::BackendTransport;
use crate::backends::types::{SessionCommand, StartupMcpServer, StartupMcpTransport};

pub const TYCODE_SUBPROCESS_VERSION: &str = env!("SUBPROCESS_VERSION");
pub const TYCODE_GIT_REPO: &str = "https://github.com/tigy32/Tycode";
pub const TYCODE_SUBPROCESS_CRATE_NAME: &str = "tycode-subprocess";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub data: String,
    pub media_type: String,
    pub name: String,
    pub size: u64,
}

pub struct TycodeSubprocessBridge {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Option<Child>>>,
    shutting_down: Arc<AtomicBool>,
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

pub fn resolve_tycode_subprocess_path() -> Result<String, String> {
    if let Ok(path) = std::env::var("TYDE_SUBPROCESS_PATH") {
        tracing::info!("Found subprocess via TYDE_SUBPROCESS_PATH env var");
        return Ok(path);
    }
    tracing::warn!("TYDE_SUBPROCESS_PATH env var not set");

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join("tycode-subprocess");
            if is_executable(&sibling) {
                tracing::info!("Found subprocess as sibling of current executable");
                return Ok(sibling.to_string_lossy().to_string());
            }
        }
    }
    tracing::warn!("Subprocess not found as sibling of current executable");

    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let installed = PathBuf::from(&home).join(format!(
            ".tycode/v{}/bin/{}",
            TYCODE_SUBPROCESS_VERSION, TYCODE_SUBPROCESS_CRATE_NAME
        ));
        if is_executable(&installed) {
            tracing::info!("Found subprocess in on-demand install location");
            return Ok(installed.to_string_lossy().to_string());
        }
    }
    tracing::warn!("Subprocess not found in on-demand install location");

    if let Ok(mut dir) = std::env::current_dir() {
        loop {
            let cargo_toml = dir.join("Cargo.toml");
            let is_workspace = fs::read_to_string(&cargo_toml)
                .map(|contents| contents.contains("[workspace]"))
                .unwrap_or(false);

            if is_workspace {
                let debug = dir.join("target/debug/tycode-subprocess");
                if is_executable(&debug) {
                    tracing::info!("Found subprocess in workspace target/debug");
                    return Ok(debug.to_string_lossy().to_string());
                }
                let release = dir.join("target/release/tycode-subprocess");
                if is_executable(&release) {
                    tracing::info!("Found subprocess in workspace target/release");
                    return Ok(release.to_string_lossy().to_string());
                }
            }

            if !dir.pop() {
                break;
            }
        }
    }
    tracing::warn!("Subprocess not found in any parent workspace target directory");

    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    if let Ok(output) = StdCommand::new(which_cmd).arg("tycode-subprocess").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                tracing::info!("Found subprocess on system PATH");
                return Ok(path);
            }
        }
    }
    tracing::warn!("Subprocess not found on system PATH");

    Err("Could not find tycode-subprocess binary. \
         Set TYDE_SUBPROCESS_PATH env var or build it with: \
         cargo build -p tycode-subprocess"
        .to_string())
}

fn detect_local_target() -> Result<String, String> {
    let os = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else if cfg!(target_os = "linux") {
        "unknown-linux-musl"
    } else if cfg!(target_os = "windows") {
        "pc-windows-msvc"
    } else {
        return Err("Unsupported operating system".to_string());
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return Err("Unsupported architecture".to_string());
    };

    Ok(format!("{arch}-{os}"))
}

pub async fn install_local_tycode_subprocess() -> Result<(), String> {
    let target = detect_local_target()?;
    let archive = format!("{TYCODE_SUBPROCESS_CRATE_NAME}-{target}.tar.xz");
    let url = format!("{TYCODE_GIT_REPO}/releases/download/v{TYCODE_SUBPROCESS_VERSION}/{archive}");

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "Could not determine home directory".to_string())?;
    let install_dir = format!("{home}/.tycode/v{TYCODE_SUBPROCESS_VERSION}/bin");

    let cmd = format!(
        "TMP=$(mktemp -d) && \
         curl -sSfL \"{url}\" | tar -xJ -C \"$TMP\" && \
         mkdir -p \"{install_dir}\" && \
         find \"$TMP\" -name \"{TYCODE_SUBPROCESS_CRATE_NAME}\" -type f -exec mv {{}} \"{install_dir}/{TYCODE_SUBPROCESS_CRATE_NAME}\" \\; && \
         chmod +x \"{install_dir}/{TYCODE_SUBPROCESS_CRATE_NAME}\" && \
         rm -rf \"$TMP\""
    );
    let output = tokio::process::Command::new("sh")
        .args(["-c", &cmd])
        .output()
        .await
        .map_err(|e| format!("Failed to run install command: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed to install tycode-subprocess v{TYCODE_SUBPROCESS_VERSION} ({target}): {stderr}"
        ));
    }
    Ok(())
}

impl TycodeSubprocessBridge {
    pub async fn spawn(
        subprocess_path: &str,
        workspace_roots: &[String],
        mcp_servers_json: Option<&str>,
        ephemeral: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        let roots_json = serde_json::to_string(workspace_roots).map_err(|e| format!("{e:?}"))?;

        let mut args = vec!["--workspace-roots".to_string(), roots_json];
        if let Some(mcp_servers_json) = mcp_servers_json {
            args.push("--mcp-servers".to_string());
            args.push(mcp_servers_json.to_string());
        }
        if ephemeral {
            args.push("--ephemeral".to_string());
        }

        let mut child = BackendTransport::Local
            .spawn_process(subprocess_path, &args, None)
            .await
            .map_err(|e| format!("Failed to spawn subprocess: {e}"))?;

        let stdin = child.stdin.take().ok_or("Failed to capture stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
        let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let tx = event_tx.clone();
        let child_for_reader = Arc::new(Mutex::new(Some(child)));
        let child_ref = child_for_reader.clone();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutting_down_reader = Arc::clone(&shutting_down);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(event) = serde_json::from_str::<ChatEvent>(&line) else {
                    tracing::warn!("Failed to parse subprocess stdout: {line}");
                    continue;
                };
                let _ = tx.send(event);
            }
            let exit_code = if shutting_down_reader.load(Ordering::Acquire) {
                Some(0)
            } else {
                match child_ref.lock().await.as_mut() {
                    Some(c) => c.try_wait().ok().flatten().and_then(|s| s.code()),
                    None => None,
                }
            };
            let exit_event = ChatEvent::SubprocessExit(SubprocessExitData { exit_code });
            let _ = tx.send(exit_event);
        });

        let stderr_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!("subprocess stderr: {line}");
                let event = ChatEvent::SubprocessStderr(line);
                let _ = stderr_tx.send(event);
            }
        });

        Ok((
            Self {
                stdin: Arc::new(Mutex::new(stdin)),
                child: child_for_reader.clone(),
                shutting_down,
            },
            event_rx,
        ))
    }

    /// Callers must clone this before dropping a std::sync::Mutex guard
    /// to avoid holding a sync lock across an async await point.
    pub fn stdin(&self) -> Arc<Mutex<ChildStdin>> {
        self.stdin.clone()
    }

    async fn send_line(&self, line: &str) -> Result<(), String> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("{e:?}"))
    }

    pub async fn is_alive(&self) -> bool {
        let mut guard = self.child.lock().await;
        let Some(child) = guard.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    pub async fn shutdown(self) {
        self.shutting_down.store(true, Ordering::Release);
        if self.is_alive().await {
            let _ = self
                .send_line(&serde_json::json!({"command": "quit"}).to_string())
                .await;
        }

        let mut child = self.child.lock().await;
        let Some(mut c) = child.take() else { return };

        match tokio::time::timeout(Duration::from_secs(2), c.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = c.kill().await;
            }
        }
        // child is taken (None) — Drop will be a no-op
    }
}

impl Drop for TycodeSubprocessBridge {
    fn drop(&mut self) {
        let Ok(mut guard) = self.child.try_lock() else {
            tracing::warn!(
                "TycodeSubprocessBridge::drop: could not acquire lock, child process may be leaked"
            );
            return;
        };
        if let Some(child) = guard.as_mut() {
            if let Err(err) = child.start_kill() {
                tracing::warn!("TycodeSubprocessBridge::drop: failed to kill child: {err}");
            }
        }
    }
}

#[derive(Clone)]
pub struct TycodeCommandHandle {
    stdin: Arc<Mutex<ChildStdin>>,
}

impl TycodeCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        let payload = tycode_payload_for_command(command);
        if payload.is_empty() {
            return Ok(());
        }
        let mut guard = self.stdin.lock().await;
        guard
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| format!("{e:?}"))
    }
}

impl CommandExecutor for TycodeCommandHandle {
    fn execute(
        &self,
        command: SessionCommand,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(TycodeCommandHandle::execute(self, command))
    }
}

pub struct TycodeSession {
    bridge: TycodeSubprocessBridge,
    steering_root: Option<TycodeSteeringRoot>,
}

impl TycodeSession {
    pub(super) fn command_handle(&self) -> TycodeCommandHandle {
        TycodeCommandHandle {
            stdin: self.bridge.stdin(),
        }
    }

    pub(super) async fn shutdown(self) {
        self.bridge.shutdown().await;
        if let Some(steering_root) = self.steering_root {
            steering_root.cleanup().await;
        }
    }

    pub(super) async fn spawn(
        launch_path: &str,
        roots: &[String],
        startup_mcp_servers: &[StartupMcpServer],
        ephemeral: bool,
        steering_content: Option<&str>,
        transport: &BackendTransport,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        let mut roots = roots.to_vec();
        let steering_root = match steering_content.filter(|c| !c.trim().is_empty()) {
            Some(content) => {
                let root = TycodeSteeringRoot::create(transport, content)?;
                roots.push(root.workspace_root());
                Some(root)
            }
            None => None,
        };
        let (bridge, rx) = TycodeSubprocessBridge::spawn(
            launch_path,
            &roots,
            tycode_mcp_servers_json(startup_mcp_servers)?.as_deref(),
            ephemeral,
        )
        .await?;
        Ok((
            Self {
                bridge,
                steering_root,
            },
            rx,
        ))
    }
}

struct TycodeSteeringRoot {
    path: String,
}

impl TycodeSteeringRoot {
    fn create(_transport: &BackendTransport, content: &str) -> Result<Self, String> {
        let root = crate::steering::write_tycode_steering_root(content)?;
        Ok(Self {
            path: root.to_string_lossy().to_string(),
        })
    }

    fn workspace_root(&self) -> String {
        self.path.clone()
    }

    async fn cleanup(self) {
        let path = PathBuf::from(&self.path);
        if let Err(e) = std::fs::remove_dir_all(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "Failed to remove Tycode steering root {}: {e}",
                    path.display()
                );
            }
        }
    }
}

pub(super) fn tycode_payload_for_command(command: SessionCommand) -> String {
    match command {
        SessionCommand::SendMessage { message, images } => {
            let payload = match images {
                Some(imgs) if !imgs.is_empty() => json!({
                    "UserInputWithImages": {
                        "text": message,
                        "images": imgs
                    }
                }),
                _ => json!({ "UserInput": message }),
            };
            format!("{payload}\n")
        }
        SessionCommand::CancelConversation => "CANCEL\n".to_string(),
        SessionCommand::GetSettings => "\"GetSettings\"\n".to_string(),
        SessionCommand::ListSessions => "\"ListSessions\"\n".to_string(),
        SessionCommand::ResumeSession { session_id } => {
            format!(
                "{}\n",
                json!({ "ResumeSession": { "session_id": session_id } })
            )
        }
        SessionCommand::DeleteSession { session_id } => {
            format!(
                "{}\n",
                json!({ "UserInput": format!("/sessions delete {session_id}") })
            )
        }
        SessionCommand::ListProfiles => "\"ListProfiles\"\n".to_string(),
        SessionCommand::SwitchProfile { profile_name } => {
            format!(
                "{}\n",
                json!({ "SwitchProfile": { "profile_name": profile_name } })
            )
        }
        SessionCommand::GetModuleSchemas => "\"GetModuleSchemas\"\n".to_string(),
        SessionCommand::ListModels => String::new(),
        SessionCommand::UpdateSettings { settings, persist } => {
            format!(
                "{}\n",
                json!({ "SaveSettings": { "settings": settings, "persist": persist } })
            )
        }
    }
}

fn tycode_mcp_servers_json(
    startup_mcp_servers: &[StartupMcpServer],
) -> Result<Option<String>, String> {
    if startup_mcp_servers.is_empty() {
        return Ok(None);
    }

    let mut out = serde_json::Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }

        match &server.transport {
            StartupMcpTransport::Http { url, headers, .. } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                let mut cfg = serde_json::Map::new();
                cfg.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                if !headers.is_empty() {
                    cfg.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .map_err(|err| format!("Failed to serialize MCP headers: {err}"))?,
                    );
                }
                out.insert(name.to_string(), Value::Object(cfg));
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                let mut cfg = serde_json::Map::new();
                cfg.insert(
                    "command".to_string(),
                    Value::String(trimmed_command.to_string()),
                );
                if !args.is_empty() {
                    cfg.insert(
                        "args".to_string(),
                        serde_json::to_value(args)
                            .map_err(|err| format!("Failed to serialize MCP args: {err}"))?,
                    );
                }
                if !env.is_empty() {
                    cfg.insert(
                        "env".to_string(),
                        serde_json::to_value(env)
                            .map_err(|err| format!("Failed to serialize MCP env: {err}"))?,
                    );
                }
                out.insert(name.to_string(), Value::Object(cfg));
            }
        }
    }

    if out.is_empty() {
        return Ok(None);
    }

    serde_json::to_string(&Value::Object(out))
        .map(Some)
        .map_err(|err| format!("Failed to serialize startup MCP servers: {err}"))
}
