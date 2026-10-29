//! Agent Client Protocol support.
//!
//! * this module — the transport: JSON-RPC over the agent's stdio, plus the
//!   client-side methods every ACP agent may call back into
//!   (`fs/*`, `terminal/*`, `session/request_permission`).
//! * [`adapter`] — the seam for agent-specific behavior the protocol doesn't
//!   cover.
//! * [`adapters`] — one implementation per known agent.
//! * [`tools`] — specification-only tool mapping shared by all adapters.

pub mod adapter;
pub mod adapters;
pub mod backend;
pub mod tools;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::backend::subprocess::{AsyncCommandGroup, AsyncGroupChild};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::backend::{StartupMcpServer, StartupMcpTransport};
const ACP_DEFAULT_FILE_LINE_LIMIT: usize = 2_000;
const ACP_DEFAULT_TERMINAL_OUTPUT_LIMIT: usize = 1_048_576;

type AcpStateChange<T> = Box<dyn FnOnce(&mut T) + Send>;

pub(super) struct AcpStateActor<T> {
    transitions: mpsc::UnboundedSender<AcpStateChange<T>>,
}

impl<T> Clone for AcpStateActor<T> {
    fn clone(&self) -> Self {
        Self {
            transitions: self.transitions.clone(),
        }
    }
}

impl<T: Send + 'static> AcpStateActor<T> {
    fn spawn(mut state: T) -> Self {
        let (transitions, mut inbox) = mpsc::unbounded_channel::<AcpStateChange<T>>();
        crate::backend::subprocess::spawn(async move {
            while let Some(change) = inbox.recv().await {
                change(&mut state);
            }
        });
        Self { transitions }
    }

    async fn with<R: Send + 'static>(
        &self,
        transition: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> R {
        let (reply, result) = oneshot::channel();
        self.transitions
            .send(Box::new(move |state| {
                if reply.is_closed() {
                    return;
                }
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| transition(state)));
                let _ = reply.send(outcome);
            }))
            .unwrap_or_else(|_| panic!("ACP state owner stopped while its adapter is live"));
        match result.await {
            Ok(Ok(value)) => value,
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            Err(_) => panic!("ACP state owner dropped a transition"),
        }
    }

    fn apply(&self, transition: impl FnOnce(&mut T) + Send + 'static) {
        let _ = self.transitions.send(Box::new(transition));
    }
}

pub enum AcpInbound {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
        origin: AcpRequestOrigin,
    },
    Stderr(String),
    Closed {
        exit_code: Option<i32>,
    },
    Barrier {
        ack: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub struct AcpRequestOrigin {
    execution: AcpRequestExecution,
    stdin: AcpStdinWriter,
}

#[derive(Clone)]
enum AcpRequestExecution {
    Local(tyde_process_env::launch::ExecutionContext),
    Remote,
}

impl AcpRequestOrigin {
    async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.stdin
            .write_line(format!(
                "{}\n",
                json!({"jsonrpc":"2.0", "id":id, "result":result})
            ))
            .await
    }

    async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), String> {
        self.stdin
            .write_line(format!(
                "{}\n",
                json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
            ))
            .await
    }
}

#[derive(Clone, Debug)]
pub struct AcpSpawnSpec {
    pub display_name: String,
    pub local_program: String,
    pub local_args: Vec<String>,
    pub remote_args: Vec<String>,
    pub local_cwd: Option<String>,
    pub remote_cwd: Option<String>,
    pub local_env: Vec<(String, String)>,
}

impl AcpSpawnSpec {
    pub fn new(
        display_name: impl Into<String>,
        local_program: impl Into<String>,
        local_args: &[&str],
    ) -> Self {
        let local_program = local_program.into();
        let local_args_vec = local_args
            .iter()
            .map(|arg| arg.to_string())
            .collect::<Vec<_>>();
        let mut remote_args = vec![local_program.clone()];
        remote_args.extend(local_args_vec.clone());

        Self {
            display_name: display_name.into(),
            local_program,
            local_args: local_args_vec,
            remote_args,
            local_cwd: None,
            remote_cwd: None,
            local_env: Vec::new(),
        }
    }

    pub fn with_local_cwd(mut self, cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        if !cwd.trim().is_empty() {
            self.local_cwd = Some(cwd);
        }
        self
    }

    pub fn with_remote_cwd(mut self, cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        if !cwd.trim().is_empty() {
            self.remote_cwd = Some(cwd);
        }
        self
    }

    pub fn with_local_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.local_env.push((name.into(), value.into()));
        self
    }
}

struct AcpRpcWaiter {
    id: u64,
    restart: bool,
    reply: oneshot::Sender<Arc<AcpRpc>>,
}

struct AcpRpcState {
    rpc: Arc<AcpRpc>,
    readers: HashSet<u64>,
    restarting: Option<u64>,
    waiting: VecDeque<AcpRpcWaiter>,
}

impl AcpRpcState {
    fn admit(&mut self) {
        while self.restarting.is_none() {
            let Some(next) = self.waiting.front() else {
                break;
            };
            if next.reply.is_closed() {
                self.waiting.pop_front();
                continue;
            }
            if next.restart && !self.readers.is_empty() {
                break;
            }
            let next = self.waiting.pop_front().expect("RPC waiter is present");
            if next.reply.send(Arc::clone(&self.rpc)).is_ok() {
                if next.restart {
                    self.restarting = Some(next.id);
                } else {
                    self.readers.insert(next.id);
                }
            }
        }
    }

    fn release(&mut self, id: u64) {
        self.waiting.retain(|waiter| waiter.id != id);
        self.readers.remove(&id);
        if self.restarting == Some(id) {
            self.restarting = None;
        }
        self.admit();
    }
}

struct AcpRpcOwner {
    state: AcpStateActor<AcpRpcState>,
    next_id: AtomicU64,
}

struct AcpRpcLease {
    state: AcpStateActor<AcpRpcState>,
    id: u64,
}

impl Drop for AcpRpcLease {
    fn drop(&mut self) {
        let id = self.id;
        self.state.apply(move |state| state.release(id));
    }
}

impl AcpRpcOwner {
    fn spawn(rpc: AcpRpc) -> Self {
        Self {
            state: AcpStateActor::spawn(AcpRpcState {
                rpc: Arc::new(rpc),
                readers: HashSet::new(),
                restarting: None,
                waiting: VecDeque::new(),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    async fn acquire(&self, restart: bool) -> (AcpRpcLease, Arc<AcpRpc>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let lease = AcpRpcLease {
            state: self.state.clone(),
            id,
        };
        let (reply, result) = oneshot::channel();
        self.state.apply(move |state| {
            state.waiting.push_back(AcpRpcWaiter { id, restart, reply });
            state.admit();
        });
        let rpc = result
            .await
            .expect("ACP RPC owner stopped while its bridge is live");
        (lease, rpc)
    }
}

pub struct AcpBridge {
    rpc: AcpRpcOwner,
    inbound_tx: mpsc::UnboundedSender<AcpInbound>,
    terminals: AcpStateActor<HashMap<String, Arc<AcpTerminal>>>,
    next_terminal_id: AtomicU64,
}

impl AcpBridge {
    pub async fn spawn(
        spec: AcpSpawnSpec,
        ssh_host: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AcpInbound>), String> {
        let (rpc, inbound_tx, inbound_rx) = AcpRpc::spawn(spec, ssh_host, None).await?;
        Ok((
            Self {
                rpc: AcpRpcOwner::spawn(rpc),
                inbound_tx,
                terminals: AcpStateActor::spawn(HashMap::new()),
                next_terminal_id: AtomicU64::new(1),
            },
            inbound_rx,
        ))
    }

    #[cfg(feature = "test-support")]
    pub(crate) async fn native_process_id_for_test(&self) -> Result<u32, String> {
        let (_lease, rpc) = self.rpc.acquire(false).await;
        rpc.child
            .native_pid
            .ok_or_else(|| "ACP native process is unavailable".to_owned())
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_typed(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn request_typed(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, AcpRequestError> {
        let (lease, rpc) = self.rpc.acquire(false).await;
        let outcome = rpc.request(method, params).await;
        drop(lease);
        outcome
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let (lease, rpc) = self.rpc.acquire(false).await;
        let outcome = rpc.notify(method, params).await;
        drop(lease);
        outcome
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        let (lease, rpc) = self.rpc.acquire(false).await;
        let outcome = rpc.respond(id, result).await;
        drop(lease);
        outcome
    }

    pub async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), String> {
        let (lease, rpc) = self.rpc.acquire(false).await;
        let outcome = rpc.respond_error(id, code, message).await;
        drop(lease);
        outcome
    }

    pub async fn sync_inbound(&self) -> Result<(), String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.inbound_tx
            .send(AcpInbound::Barrier { ack: ack_tx })
            .map_err(|_| "ACP inbound channel closed".to_string())?;
        ack_rx
            .await
            .map_err(|_| "ACP inbound barrier dropped".to_string())
    }

    pub(crate) async fn restart_local(
        &self,
        spec: AcpSpawnSpec,
        prepare: impl std::future::Future<Output = Result<(), String>> + Send,
    ) -> Result<(), String> {
        let (lease, rpc) = self.rpc.acquire(true).await;
        if !rpc.pending.with(|pending| pending.is_empty()).await {
            return Err("ACP process has outstanding requests and cannot restart".to_owned());
        }
        // An intentional retirement must not close the stable session event stream.
        rpc.stdout_task.abort();
        rpc.stderr_task.abort();
        rpc.shutdown().await;
        prepare.await?;
        let (replacement, _, _) = AcpRpc::spawn(spec, None, Some(self.inbound_tx.clone())).await?;
        self.rpc
            .state
            .apply(move |state| state.rpc = Arc::new(replacement));
        drop(lease);
        Ok(())
    }

    pub async fn trace_terminal_state(&self, reason: &str) {
        let terminals = self
            .terminals
            .with(|terminals| terminals.values().cloned().collect::<Vec<_>>())
            .await;
        eprintln!(
            "TYDE ACP TERMINAL STATE reason={reason:?} terminal_count={}",
            terminals.len()
        );
        for terminal in terminals {
            let status = terminal.child.inspect().await;
            let (exit_code, exit_signal) = status
                .as_ref()
                .map(exit_status_pair)
                .unwrap_or((None, None));
            eprintln!(
                "TYDE ACP TERMINAL STATE reason={reason:?} child_available={} exit_code={exit_code:?} exit_signal={exit_signal:?}",
                status.is_none(),
            );
        }
    }

    pub async fn kill_active_terminals(&self) -> Result<(), String> {
        let terminals = self
            .terminals
            .with(|terminals| terminals.values().cloned().collect::<Vec<_>>())
            .await;
        for terminal in terminals {
            kill_terminal(terminal).await?;
        }
        Ok(())
    }

    pub async fn handle_server_request(
        &self,
        origin: &AcpRequestOrigin,
        id: Value,
        method: &str,
        params: &Value,
    ) -> Result<bool, String> {
        let result = match self
            .handle_builtin_request(&origin.execution, method, params)
            .await
        {
            Ok(Some(result)) => result,
            Ok(None) => return Ok(false),
            Err(err) => {
                origin.respond_error(id, -32_000, &err).await?;
                return Ok(true);
            }
        };

        origin.respond(id, result).await?;
        Ok(true)
    }

    pub async fn shutdown(&self) {
        let terminals = self
            .terminals
            .with(|map| {
                map.drain()
                    .map(|(_, terminal)| terminal)
                    .collect::<Vec<_>>()
            })
            .await;
        for terminal in terminals {
            let _ = terminate_terminal(terminal).await;
        }
        let (lease, rpc) = self.rpc.acquire(false).await;
        rpc.shutdown().await;
        drop(lease);
    }

    async fn handle_builtin_request(
        &self,
        execution: &AcpRequestExecution,
        method: &str,
        params: &Value,
    ) -> Result<Option<Value>, String> {
        match (execution, method) {
            (
                AcpRequestExecution::Remote,
                "fs/read_text_file"
                | "fs/write_text_file"
                | "terminal/create"
                | "terminal/output"
                | "terminal/wait_for_exit"
                | "terminal/kill"
                | "terminal/release",
            ) => {
                tracing::warn!(
                    method,
                    "Rejected unsupported remote ACP filesystem or terminal callback"
                );
                Err("Remote ACP filesystem and terminal callbacks are unsupported".to_owned())
            }
            (AcpRequestExecution::Local(execution), "fs/read_text_file") => self
                .handle_fs_read_text_file(execution, params)
                .await
                .map(Some),
            (AcpRequestExecution::Local(execution), "fs/write_text_file") => self
                .handle_fs_write_text_file(execution, params)
                .await
                .map(Some),
            (AcpRequestExecution::Local(execution), "terminal/create") => self
                .handle_terminal_create(execution, params)
                .await
                .map(Some),
            (AcpRequestExecution::Local(_), "terminal/output") => {
                self.handle_terminal_output(params).await.map(Some)
            }
            (AcpRequestExecution::Local(_), "terminal/wait_for_exit") => {
                self.handle_terminal_wait_for_exit(params).await.map(Some)
            }
            (AcpRequestExecution::Local(_), "terminal/kill") => {
                self.handle_terminal_kill(params).await.map(Some)
            }
            (AcpRequestExecution::Local(_), "terminal/release") => {
                self.handle_terminal_release(params).await.map(Some)
            }
            (_, "session/request_permission") => {
                self.handle_request_permission(params).await.map(Some)
            }
            _ => Ok(None),
        }
    }

    async fn handle_fs_read_text_file(
        &self,
        execution: &tyde_process_env::launch::ExecutionContext,
        params: &Value,
    ) -> Result<Value, String> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or("fs/read_text_file requires non-empty 'path'")?;

        if !Path::new(path).is_absolute() {
            return Err("fs/read_text_file requires an absolute path".to_string());
        }

        let line = params
            .get("line")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(ACP_DEFAULT_FILE_LINE_LIMIT as u64)
            .max(1) as usize;

        let read = execution.read(Path::new(path)).await;
        tracing::info!(
            route = ?execution.route(),
            success = read.is_ok(),
            "ACP caller filesystem read completed"
        );
        let raw_bytes = read.map_err(|err| format!("Failed to read file '{path}': {err}"))?;
        let content = String::from_utf8_lossy(&raw_bytes).to_string();

        let lines = content.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start_idx = line.saturating_sub(1).min(total_lines);
        let end_idx = start_idx.saturating_add(limit).min(total_lines);
        let sliced = if start_idx < end_idx {
            lines[start_idx..end_idx].join("\n")
        } else {
            String::new()
        };

        Ok(json!({
            "content": sliced,
            "totalLines": total_lines,
            "startLine": if total_lines == 0 { 1 } else { start_idx + 1 },
            "isPartial": start_idx > 0 || end_idx < total_lines,
        }))
    }

    async fn handle_fs_write_text_file(
        &self,
        execution: &tyde_process_env::launch::ExecutionContext,
        params: &Value,
    ) -> Result<Value, String> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or("fs/write_text_file requires non-empty 'path'")?;

        if !Path::new(path).is_absolute() {
            return Err("fs/write_text_file requires an absolute path".to_string());
        }

        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or("fs/write_text_file requires string 'content'")?;

        let written = execution.write(Path::new(path), content.as_bytes()).await;
        tracing::info!(
            route = ?execution.route(),
            success = written.is_ok(),
            "ACP caller filesystem write completed"
        );
        written.map_err(|err| format!("Failed to write file '{path}': {err}"))?;

        Ok(Value::Null)
    }

    async fn handle_terminal_create(
        &self,
        execution: &tyde_process_env::launch::ExecutionContext,
        params: &Value,
    ) -> Result<Value, String> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|cmd| !cmd.is_empty())
            .ok_or("terminal/create requires non-empty 'command'")?;

        let mut args = params
            .get("args")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let (command, normalized_args) = normalize_grok_shell_command(command, &args);
        if let Some(normalized_args) = normalized_args {
            args = normalized_args;
        }

        let env_vars = params
            .get("env")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|entry| {
                        let name = entry.get("name").and_then(Value::as_str)?.trim();
                        if name.is_empty() {
                            return None;
                        }
                        let value = entry
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        Some((name.to_string(), value))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string());

        if let Some(path) = cwd.as_ref()
            && !Path::new(path).is_absolute()
        {
            return Err("terminal/create requires absolute 'cwd'".to_string());
        }

        let output_limit = params
            .get("outputByteLimit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(ACP_DEFAULT_TERMINAL_OUTPUT_LIMIT)
            .max(1);

        let mut cmd = execution.command(command)?;
        cmd.args(&args)
            .stdin(crate::backend::subprocess::ProcessStdio::Null)
            .stdout(crate::backend::subprocess::ProcessStdio::Piped)
            .stderr(crate::backend::subprocess::ProcessStdio::Piped);

        if let Some(path) = cwd.as_ref() {
            cmd.current_dir(path);
        }
        for (name, value) in env_vars {
            cmd.env(name, value);
        }

        let mut child = cmd
            .group_spawn()
            .await
            .map_err(|err| format!("Failed to spawn terminal command '{command}': {err}"))?;
        tracing::info!(route = ?execution.route(), "ACP caller command launched");

        let stdout = child.take_stdout();
        let stderr = child.take_stderr();

        let terminal = Arc::new(AcpTerminal {
            child: AcpChildOwner::spawn(child),
            output: AcpStateActor::spawn(AcpTerminalOutput {
                output: Vec::new(),
                output_limit,
                truncated: false,
            }),
        });

        if let Some(reader) = stdout {
            crate::backend::subprocess::spawn(read_terminal_output(reader, Arc::clone(&terminal)));
        }
        if let Some(reader) = stderr {
            crate::backend::subprocess::spawn(read_terminal_output(reader, Arc::clone(&terminal)));
        }

        let terminal_id = format!(
            "term-{}",
            self.next_terminal_id.fetch_add(1, Ordering::Relaxed)
        );
        let registered_id = terminal_id.clone();
        self.terminals.apply(move |terminals| {
            terminals.insert(registered_id, terminal);
        });

        Ok(json!({ "terminalId": terminal_id }))
    }

    async fn handle_terminal_output(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .with({
                let terminal_id = terminal_id.clone();
                move |terminals| terminals.get(&terminal_id).cloned()
            })
            .await
            .ok_or_else(|| format!("Unknown terminalId '{terminal_id}'"))?;

        let status = terminal.child.inspect().await;
        let (output, truncated) = terminal
            .output
            .with(|state| {
                (
                    String::from_utf8_lossy(&state.output).to_string(),
                    state.truncated,
                )
            })
            .await;
        let mut payload = json!({ "output": output, "truncated": truncated });
        if let Some(status) = status {
            let (exit_code, signal) = exit_status_pair(&status);
            payload["exitStatus"] = json!({ "exitCode": exit_code, "signal": signal });
        }
        Ok(payload)
    }

    async fn handle_terminal_wait_for_exit(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .with({
                let terminal_id = terminal_id.clone();
                move |terminals| terminals.get(&terminal_id).cloned()
            })
            .await
            .ok_or_else(|| format!("Unknown terminalId '{terminal_id}'"))?;

        let (exit_code, signal) = wait_for_terminal_exit(terminal).await?;
        Ok(json!({
            "exitCode": exit_code,
            "signal": signal,
        }))
    }

    async fn handle_terminal_kill(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .with({
                let terminal_id = terminal_id.clone();
                move |terminals| terminals.get(&terminal_id).cloned()
            })
            .await
            .ok_or_else(|| format!("Unknown terminalId '{terminal_id}'"))?;

        kill_terminal(terminal).await?;
        Ok(Value::Null)
    }

    async fn handle_terminal_release(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .with({
                let terminal_id = terminal_id.clone();
                move |terminals| terminals.remove(&terminal_id)
            })
            .await
            .ok_or_else(|| format!("Unknown terminalId '{terminal_id}'"))?;

        terminate_terminal(terminal).await?;
        Ok(Value::Null)
    }

    async fn handle_request_permission(&self, params: &Value) -> Result<Value, String> {
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let selected = select_permission_option(&options);
        if let Some(option_id) = selected {
            Ok(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id,
                }
            }))
        } else {
            Ok(json!({
                "outcome": {
                    "outcome": "cancelled"
                }
            }))
        }
    }
}

fn normalize_grok_shell_command<'a>(
    command: &'a str,
    args: &[String],
) -> (&'a str, Option<Vec<String>>) {
    if !args.is_empty() {
        return (command, None);
    }
    for shell in ["/bin/bash", "/usr/bin/bash", "/bin/zsh", "/bin/sh"] {
        let prefix = format!("{shell} -lc ");
        let Some(raw_script) = command.strip_prefix(&prefix) else {
            continue;
        };
        let script = raw_script.trim();
        let script = if script.len() >= 2
            && ((script.starts_with('"') && script.ends_with('"'))
                || (script.starts_with('\'') && script.ends_with('\'')))
        {
            &script[1..script.len() - 1]
        } else {
            script
        };
        return (
            shell,
            Some(vec!["-lc".to_owned(), script.replace("\\\"", "\"")]),
        );
    }
    (command, None)
}

#[derive(Clone)]
pub struct AcpToolCallRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
}

#[derive(Clone)]
pub struct AcpToolCallCompletion {
    pub tool_call_id: String,
    pub tool_name: String,
    pub kind: String,
    pub success: bool,
    pub tool_result: Value,
    pub error: Option<String>,
    pub is_mcp_tool: bool,
}

pub fn parse_tool_call_request(params: &Value) -> Option<AcpToolCallRequest> {
    let tool_call_id = extract_tool_call_id(params)?;
    let tool_name = params
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let args = params
        .get("rawInput")
        .cloned()
        .map(normalize_tool_call_args)
        .unwrap_or(Value::Object(Default::default()));

    Some(AcpToolCallRequest {
        tool_call_id,
        tool_name,
        args,
    })
}

fn normalize_tool_call_args(raw: Value) -> Value {
    let mut value = decode_embedded_json(raw);

    for _ in 0..5 {
        let Some(obj) = value.as_object() else {
            break;
        };

        let nested = obj
            .get("input")
            .or_else(|| obj.get("toolInput"))
            .or_else(|| obj.get("tool_input"))
            .or_else(|| obj.get("args"))
            .or_else(|| obj.get("arguments"))
            .or_else(|| obj.get("params"))
            .or_else(|| obj.get("request"))
            .or_else(|| obj.get("payload"))
            .or_else(|| obj.get("rawInput"))
            .or_else(|| obj.get("raw_input"));

        let Some(next) = nested.cloned() else {
            break;
        };

        let decoded = decode_embedded_json(next);
        if decoded == value {
            break;
        }
        value = decoded;
    }

    value
}

fn decode_embedded_json(value: Value) -> Value {
    if let Value::String(text) = &value {
        let trimmed = text.trim();
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
        {
            return parsed;
        }
    }
    value
}

pub fn parse_tool_call_completion(
    params: &Value,
    fallback_tool_name: Option<String>,
) -> Option<AcpToolCallCompletion> {
    let tool_call_id = extract_tool_call_id(params)?;
    let status = params
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    if status.is_empty() || status == "pending" || status == "in_progress" {
        return None;
    }

    let tool_name = params
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or(fallback_tool_name)
        .unwrap_or_else(|| "tool".to_string());
    let kind = params
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let tool_result = params
        .get("rawOutput")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let success =
        (status == "completed" || status == "success") && !value_contains_mcp_error(&tool_result);

    let error = if success {
        None
    } else {
        params
            .get("error")
            .and_then(|v| {
                if let Some(message) = v.get("message").and_then(Value::as_str) {
                    return Some(message.to_string());
                }
                v.as_str().map(|s| s.to_string())
            })
            .or_else(|| Some(format!("{tool_name} failed")))
    };

    Some(AcpToolCallCompletion {
        tool_call_id,
        tool_name,
        kind,
        success,
        tool_result,
        error,
        is_mcp_tool: false,
    })
}

fn value_contains_mcp_error(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("isError")
                .or_else(|| object.get("is_error"))
                .and_then(Value::as_bool)
                == Some(true)
                || object.values().any(value_contains_mcp_error)
        }
        Value::Array(values) => values.iter().any(value_contains_mcp_error),
        _ => false,
    }
}

pub fn extract_text_from_update(update: &Value) -> String {
    if let Some(text) = update.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        return text.to_string();
    }

    if let Some(delta) = update.get("delta").and_then(Value::as_str)
        && !delta.is_empty()
    {
        return delta.to_string();
    }

    if let Some(content) = update.get("content") {
        return extract_text_from_content(content);
    }

    if let Some(chunks) = update.get("chunks") {
        return extract_text_from_content(chunks);
    }

    String::new()
}

fn extract_text_from_content(content: &Value) -> String {
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return text.to_string();
    }

    if let Some(content_type) = content.get("type").and_then(Value::as_str)
        && content_type == "content"
        && let Some(inner) = content.get("content")
    {
        return extract_text_from_content(inner);
    }

    if let Some(array) = content.as_array() {
        let mut chunks = Vec::new();
        for item in array {
            let text = extract_text_from_content(item);
            if !text.is_empty() {
                chunks.push(text);
            }
        }
        return chunks.join("\n");
    }

    String::new()
}

pub fn extract_message_id(value: &Value) -> Option<String> {
    value
        .get("messageId")
        .or_else(|| value.get("message_id"))
        .or_else(|| value.get("id"))
        .or_else(|| value.get("itemId"))
        .and_then(Value::as_str)
        .map(|raw| raw.to_string())
}

pub fn extract_tool_call_id(value: &Value) -> Option<String> {
    value
        .get("toolCallId")
        .or_else(|| value.get("tool_call_id"))
        .or_else(|| value.get("callId"))
        .or_else(|| value.get("call_id"))
        .or_else(|| {
            value
                .get("toolCall")
                .and_then(|tool| tool.get("toolCallId"))
        })
        .and_then(Value::as_str)
        .map(|raw| raw.to_string())
}

pub fn normalize_update_type(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

pub fn map_plan_status(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pending" | "todo" => "pending",
        "in_progress" | "in-progress" | "active" | "running" => "in_progress",
        "done" | "completed" | "complete" | "success" => "completed",
        "failed" | "error" | "cancelled" => "failed",
        _ => "pending",
    }
}

pub fn acp_mcp_servers_json(startup_mcp_servers: &[StartupMcpServer]) -> Vec<Value> {
    let mut servers = Vec::new();

    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }

        match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }

                let header_values = headers
                    .iter()
                    .map(|(header_name, header_value)| {
                        json!({
                            "name": header_name,
                            "value": header_value,
                        })
                    })
                    .collect::<Vec<_>>();

                let mut value = json!({
                    "type": "http",
                    "name": name,
                    "url": trimmed_url,
                    "headers": header_values,
                });

                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    value["bearerTokenEnvVar"] = Value::String(env_var.to_string());
                }

                servers.push(value);
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }

                let env_vars = env
                    .iter()
                    .map(|(env_name, env_value)| {
                        json!({
                            "name": env_name,
                            "value": env_value,
                        })
                    })
                    .collect::<Vec<_>>();

                servers.push(json!({
                    "type": "stdio",
                    "name": name,
                    "command": trimmed_command,
                    "args": args,
                    "env": env_vars,
                }));
            }
        }
    }

    servers
}

enum AcpChildCommand {
    Inspect(oneshot::Sender<Option<std::process::ExitStatus>>),
    Kill(oneshot::Sender<Result<(), String>>),
    Shutdown(Option<oneshot::Sender<()>>),
}

#[derive(Clone)]
struct AcpChildOwner {
    commands: mpsc::UnboundedSender<AcpChildCommand>,
    #[cfg(feature = "test-support")]
    native_pid: Option<u32>,
}

struct AcpChildResource(Option<AsyncGroupChild>);

impl Drop for AcpChildResource {
    fn drop(&mut self) {
        if let Some(child) = self.0.take() {
            crate::backend::subprocess::reap_group_child(child);
        }
    }
}

impl AcpChildOwner {
    fn spawn(child: AsyncGroupChild) -> Self {
        #[cfg(feature = "test-support")]
        let native_pid = child.id();
        let (commands, mut inbox) = mpsc::unbounded_channel();
        crate::backend::subprocess::spawn(async move {
            let mut resource = AcpChildResource(Some(child));
            let mut exit_status = None;
            while let Some(command) = inbox.recv().await {
                match command {
                    AcpChildCommand::Inspect(reply) => {
                        if let Some(child) = resource.0.as_mut()
                            && let Ok(Some(status)) = child.try_wait()
                        {
                            exit_status = Some(status);
                            resource.0 = None;
                        }
                        let _ = reply.send(exit_status);
                    }
                    AcpChildCommand::Kill(reply) => {
                        let outcome = if let Some(child) = resource.0.as_mut() {
                            match child.kill().await {
                                Ok(()) => match child.wait().await {
                                    Ok(status) => {
                                        exit_status = Some(status);
                                        resource.0 = None;
                                        Ok(())
                                    }
                                    Err(err) => Err(format!(
                                        "Failed to wait for killed terminal process: {err}"
                                    )),
                                },
                                Err(err) => Err(format!("Failed to kill terminal process: {err}")),
                            }
                        } else {
                            Ok(())
                        };
                        let _ = reply.send(outcome);
                    }
                    AcpChildCommand::Shutdown(reply) => {
                        if let Some(child) = resource.0.as_mut() {
                            let _ = child.start_kill();
                            if let Ok(status) = child.wait().await {
                                exit_status = Some(status);
                            }
                            resource.0 = None;
                        }
                        if let Some(reply) = reply {
                            let _ = reply.send(());
                        }
                    }
                }
            }
        });
        Self {
            commands,
            #[cfg(feature = "test-support")]
            native_pid,
        }
    }

    async fn inspect(&self) -> Option<std::process::ExitStatus> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(AcpChildCommand::Inspect(reply))
            .unwrap_or_else(|_| panic!("ACP child owner stopped while its process handle is live"));
        result.await.expect("ACP child owner dropped status reply")
    }

    async fn kill(&self) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(AcpChildCommand::Kill(reply))
            .map_err(|_| "Failed to kill terminal process: process owner closed".to_owned())?;
        result
            .await
            .map_err(|_| "Failed to kill terminal process: process owner closed".to_owned())?
    }

    async fn shutdown(&self) {
        let (reply, result) = oneshot::channel();
        if self
            .commands
            .send(AcpChildCommand::Shutdown(Some(reply)))
            .is_ok()
        {
            let _ = result.await;
        }
    }

    fn retire(&self) {
        let _ = self.commands.send(AcpChildCommand::Shutdown(None));
    }
}

struct AcpTerminal {
    child: AcpChildOwner,
    output: AcpStateActor<AcpTerminalOutput>,
}

struct AcpTerminalOutput {
    output: Vec<u8>,
    output_limit: usize,
    truncated: bool,
}

impl Drop for AcpTerminal {
    fn drop(&mut self) {
        self.child.retire();
    }
}

async fn read_terminal_output<R>(mut reader: R, terminal: Arc<AcpTerminal>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(err) => {
                terminal.output.apply(move |state| {
                    append_output_to_terminal(
                        state,
                        format!("\n[terminal stream read error: {err}]\n").as_bytes(),
                    );
                });
                return;
            }
        };
        let chunk = buf[..read].to_vec();
        terminal
            .output
            .with(move |state| append_output_to_terminal(state, &chunk))
            .await;
    }
}

fn append_output_to_terminal(terminal: &mut AcpTerminalOutput, chunk: &[u8]) {
    let output_limit = terminal.output_limit;
    append_terminal_output(
        &mut terminal.output,
        chunk,
        output_limit,
        &mut terminal.truncated,
    );
}

fn append_terminal_output(output: &mut Vec<u8>, chunk: &[u8], limit: usize, truncated: &mut bool) {
    if limit == 0 {
        return;
    }

    if chunk.len() >= limit {
        output.clear();
        output.extend_from_slice(&chunk[chunk.len() - limit..]);
        trim_leading_utf8_continuations(output);
        *truncated = true;
        return;
    }

    let overflow = output
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(limit);
    if overflow > 0 {
        output.drain(0..overflow);
        trim_leading_utf8_continuations(output);
        *truncated = true;
    }

    output.extend_from_slice(chunk);
}

fn trim_leading_utf8_continuations(output: &mut Vec<u8>) {
    while let Some(first) = output.first().copied() {
        if (first & 0b1100_0000) != 0b1000_0000 {
            break;
        }
        output.remove(0);
    }
}

async fn wait_for_terminal_exit(
    terminal: Arc<AcpTerminal>,
) -> Result<(Option<i32>, Option<String>), String> {
    loop {
        if let Some(status) = terminal.child.inspect().await {
            return Ok(exit_status_pair(&status));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn kill_terminal(terminal: Arc<AcpTerminal>) -> Result<(), String> {
    terminal.child.kill().await
}

async fn terminate_terminal(terminal: Arc<AcpTerminal>) -> Result<(), String> {
    kill_terminal(terminal).await
}

#[cfg(unix)]
fn exit_status_pair(status: &std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    use std::os::unix::process::ExitStatusExt;

    let exit_code = status.code();
    let signal = status.signal().map(|sig| sig.to_string());
    (exit_code, signal)
}

#[cfg(not(unix))]
fn exit_status_pair(status: &std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    (status.code(), None)
}

fn terminal_id_from_params(params: &Value) -> Result<String, String> {
    params
        .get("terminalId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or("terminal method requires non-empty 'terminalId'".to_string())
}

fn select_permission_option(options: &[Value]) -> Option<String> {
    let find_by_kind = |kind: &str| {
        options.iter().find_map(|option| {
            let option_kind = option
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if option_kind == kind {
                option
                    .get("optionId")
                    .or_else(|| option.get("option_id"))
                    .and_then(Value::as_str)
                    .map(|value| value.to_string())
            } else {
                None
            }
        })
    };

    find_by_kind("allow_once")
        .or_else(|| find_by_kind("allow_always"))
        .or_else(|| {
            options.iter().find_map(|option| {
                option
                    .get("optionId")
                    .or_else(|| option.get("option_id"))
                    .and_then(Value::as_str)
                    .map(|value| value.to_string())
            })
        })
}

#[derive(Debug)]
pub(crate) enum AcpRequestError {
    Rejected(String),
    Transport(String),
}

impl std::fmt::Display for AcpRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) | Self::Transport(message) => formatter.write_str(message),
        }
    }
}

const ACP_STDIN_CLOSED: &str = "Acp process stdin is closed";

struct AcpStdinWrite {
    line: String,
    written: oneshot::Sender<Result<(), String>>,
}

/// Sole owner of the process's stdin. Lines are written whole, in the
/// order they were submitted; a close drops stdin even while a write is
/// blocked on a full pipe, so teardown never waits behind the child.
#[derive(Clone)]
struct AcpStdinWriter {
    writes: mpsc::UnboundedSender<AcpStdinWrite>,
    closes: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl AcpStdinWriter {
    fn spawn(stdin: ChildStdin) -> Self {
        let (writes, write_rx) = mpsc::unbounded_channel();
        let (closes, close_rx) = mpsc::unbounded_channel();
        crate::backend::subprocess::spawn(run_acp_stdin_writer(stdin, write_rx, close_rx));
        Self { writes, closes }
    }

    fn enqueue_line(&self, line: String) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (written, result) = oneshot::channel();
        self.writes
            .send(AcpStdinWrite { line, written })
            .map_err(|_| ACP_STDIN_CLOSED.to_owned())?;
        Ok(result)
    }

    async fn write_line(&self, line: String) -> Result<(), String> {
        self.enqueue_line(line)?
            .await
            .unwrap_or_else(|_| Err(ACP_STDIN_CLOSED.to_owned()))
    }

    async fn close(&self) {
        let (closed, done) = oneshot::channel();
        if self.closes.send(closed).is_ok() {
            let _ = done.await;
        }
    }
}

async fn run_acp_stdin_writer(
    mut stdin: ChildStdin,
    mut writes: mpsc::UnboundedReceiver<AcpStdinWrite>,
    mut closes: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
) {
    let closed = loop {
        let write = tokio::select! {
            biased;
            closed = closes.recv() => break closed,
            write = writes.recv() => match write {
                Some(write) => write,
                None => break None,
            },
        };
        tokio::select! {
            biased;
            closed = closes.recv() => {
                let _ = write.written.send(Err(ACP_STDIN_CLOSED.to_owned()));
                break closed;
            }
            result = stdin.write_all(write.line.as_bytes()) => {
                let _ = write.written.send(
                    result.map_err(|error| format!("Failed to write to Acp stdin: {error}")),
                );
            }
        }
    };
    drop(stdin);
    writes.close();
    if let Some(closed) = closed {
        let _ = closed.send(());
    }
    while let Ok(closed) = closes.try_recv() {
        let _ = closed.send(());
    }
}

type PendingRpcMap = AcpStateActor<HashMap<u64, oneshot::Sender<Result<Value, AcpRequestError>>>>;

struct AcpPendingRequest {
    pending: PendingRpcMap,
    id: u64,
}

impl Drop for AcpPendingRequest {
    fn drop(&mut self) {
        let id = self.id;
        self.pending.apply(move |pending| {
            pending.remove(&id);
        });
    }
}

struct AcpRpc {
    stdin: AcpStdinWriter,
    pending: PendingRpcMap,
    next_id: AtomicU64,
    child: AcpChildOwner,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

impl AcpRpc {
    async fn spawn(
        spec: AcpSpawnSpec,
        ssh_host: Option<&str>,
        existing_inbound: Option<mpsc::UnboundedSender<AcpInbound>>,
    ) -> Result<
        (
            Self,
            mpsc::UnboundedSender<AcpInbound>,
            mpsc::UnboundedReceiver<AcpInbound>,
        ),
        String,
    > {
        let (mut child, execution) = if let Some(host) = ssh_host {
            let child = crate::remote::spawn_remote_process(
                host,
                &spec.remote_args[0],
                &spec.remote_args[1..],
                spec.remote_cwd.as_deref(),
            )
            .await
            .map_err(|err| format!("Failed to spawn {} over SSH: {err}", spec.display_name))?;
            (child, AcpRequestExecution::Remote)
        } else {
            let mut cmd = crate::backend::subprocess::command(&spec.local_program)?;
            cmd.args(&spec.local_args)
                .stdin(crate::backend::subprocess::ProcessStdio::Piped)
                .stdout(crate::backend::subprocess::ProcessStdio::Piped)
                .stderr(crate::backend::subprocess::ProcessStdio::Piped);
            cmd.envs(spec.local_env);
            if let Some(path) = spec
                .local_cwd
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                cmd.current_dir(path);
            }
            let (child, execution) = cmd
                .group_spawn_with_execution()
                .await
                .map_err(|err| format!("Failed to spawn {}: {err}", spec.display_name))?;
            (child, AcpRequestExecution::Local(execution))
        };

        let stdin = child.take_stdin().ok_or("Failed to capture ACP stdin")?;
        let stdout = child.take_stdout().ok_or("Failed to capture ACP stdout")?;
        let stderr = child.take_stderr().ok_or("Failed to capture ACP stderr")?;

        let input = AcpStdinWriter::spawn(stdin);
        let request_origin = AcpRequestOrigin {
            execution,
            stdin: input.clone(),
        };
        let child_ref = AcpChildOwner::spawn(child);
        let pending = PendingRpcMap::spawn(HashMap::new());
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let inbound_tx = existing_inbound.unwrap_or(inbound_tx);

        let stdout_pending = pending.clone();
        let stdout_inbound = inbound_tx.clone();
        let stdout_child = child_ref.clone();
        let stdout_task = crate::backend::subprocess::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let parsed = match serde_json::from_str::<Value>(&line) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(error = %err, bytes = line.len(), "Failed to parse ACP stdout JSON");
                        continue;
                    }
                };

                if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
                    let has_method = parsed.get("method").is_some();
                    let has_result_or_error =
                        parsed.get("result").is_some() || parsed.get("error").is_some();
                    if has_result_or_error && !has_method {
                        let response = if let Some(result) = parsed.get("result") {
                            Ok(result.clone())
                        } else {
                            let err_obj = parsed.get("error").cloned().unwrap_or(Value::Null);
                            let base_message = err_obj
                                .get("message")
                                .and_then(Value::as_str)
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| format!("ACP JSON-RPC error: {err_obj}"));
                            let code = err_obj.get("code").and_then(Value::as_i64);
                            let details = err_obj.get("data").map(|value| {
                                if let Some(text) = value.as_str() {
                                    text.to_string()
                                } else {
                                    serde_json::to_string(value)
                                        .unwrap_or_else(|_| value.to_string())
                                }
                            });

                            let mut message = base_message;
                            if let Some(code) = code {
                                message = format!("{message} (code {code})");
                            }
                            if let Some(details) = details
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty())
                            {
                                message = format!("{message}: {details}");
                            }

                            Err(AcpRequestError::Rejected(message))
                        };

                        stdout_pending
                            .with(move |pending| {
                                if let Some(tx) = pending.remove(&id) {
                                    let _ = tx.send(response);
                                }
                            })
                            .await;
                        continue;
                    }
                }

                if let Some(method) = parsed.get("method").and_then(Value::as_str) {
                    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(id) = parsed.get("id").cloned() {
                        let _ = stdout_inbound.send(AcpInbound::ServerRequest {
                            id,
                            method: method.to_string(),
                            params,
                            origin: request_origin.clone(),
                        });
                    } else {
                        let _ = stdout_inbound.send(AcpInbound::Notification {
                            method: method.to_string(),
                            params,
                        });
                    }
                }
            }

            let exit_code = stdout_child
                .inspect()
                .await
                .and_then(|status| status.code());
            stdout_pending
                .with(|pending| {
                    for (_, tx) in pending.drain() {
                        let _ = tx.send(Err(AcpRequestError::Transport(
                            "ACP process exited before response".to_string(),
                        )));
                    }
                })
                .await;

            let _ = stdout_inbound.send(AcpInbound::Closed { exit_code });
        });

        let stderr_inbound = inbound_tx.clone();
        let stderr_task = crate::backend::subprocess::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_inbound.send(AcpInbound::Stderr(line));
            }
        });

        Ok((
            Self {
                stdin: input,
                pending,
                next_id: AtomicU64::new(1),
                child: child_ref,
                stdout_task,
                stderr_task,
            },
            inbound_tx,
            inbound_rx,
        ))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, AcpRequestError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        let registration = AcpPendingRequest {
            pending: self.pending.clone(),
            id,
        };
        self.pending.apply(move |pending| {
            pending.insert(id, tx);
        });

        let written = match self.stdin.enqueue_line(format!("{payload}\n")) {
            Ok(written) => written,
            Err(err) => {
                drop(registration);
                return Err(AcpRequestError::Transport(err));
            }
        };
        let (reply, result) = oneshot::channel();
        // A submitted request stays outstanding even if its caller disappears:
        // restart must still refuse it until the provider answers or exits.
        crate::backend::subprocess::spawn(async move {
            let outcome = match written
                .await
                .unwrap_or_else(|_| Err(ACP_STDIN_CLOSED.to_owned()))
            {
                Ok(()) => match rx.await {
                    Ok(result) => result,
                    Err(_) => Err(AcpRequestError::Transport(
                        "ACP response channel closed".to_string(),
                    )),
                },
                Err(err) => Err(AcpRequestError::Transport(err)),
            };
            drop(registration);
            let _ = reply.send(outcome);
        });
        result.await.unwrap_or_else(|_| {
            Err(AcpRequestError::Transport(
                "ACP response channel closed".to_string(),
            ))
        })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await
    }

    async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), String> {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            }
        }))
        .await
    }

    async fn send_json(&self, value: &Value) -> Result<(), String> {
        self.stdin.write_line(format!("{value}\n")).await
    }

    async fn shutdown(&self) {
        // Close stdin to propagate EOF through SSH to the remote process.
        // SIGKILL on the local SSH slave doesn't give it time to send
        // "channel close" through the ControlMaster, so the remote
        // process never learns its client disconnected and becomes an
        // orphan (PPID=1). Closing stdin first lets the SSH slave
        // cleanly tear down the channel.
        self.stdin.close().await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        self.child.shutdown().await;
        // child is taken (None) — Drop will be a no-op. Drop the readers so the
        // parent-side stdio pipe fds are released even if EOF hasn't propagated.
        self.stdout_task.abort();
        self.stderr_task.abort();
    }
}

impl Drop for AcpRpc {
    /// Safety net for disconnect/panic/teardown when `shutdown()` didn't run.
    /// Aborts the reader tasks (closing the parent-side pipe fds they hold) and
    /// reaps the child so it doesn't linger as a zombie. A no-op once the child
    /// has already been taken by `shutdown()`.
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        self.child.retire();
    }
}
