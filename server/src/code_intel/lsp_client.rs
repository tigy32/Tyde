//! A generic LSP client over a grouped child process.
//!
//! Responsibilities:
//!
//! - own the child subprocess (group-spawned via the verified
//!   `backend::subprocess` reaping path — we do **not** reimplement reaping),
//! - frame traffic with the `Content-Length` codec ([`super::lsp_codec`]),
//! - correlate client→server **requests** with their responses via an internal
//!   request-id → `oneshot` map, and
//! - deliver server→client **notifications** (`publishDiagnostics`,
//!   `$/progress`, …) on an `mpsc` channel.
//!
//! The numeric LSP request ids are an **internal** correlation detail — they
//! never leave this module and never appear on the Tyde wire protocol (which is
//! event-stream, not request/response). Server→client *requests* (e.g.
//! `window/workDoneProgress/create`, `workspace/configuration`) are
//! auto-answered with conservative defaults so rust-analyzer never blocks
//! waiting on us.
//!
//! Design is an actor (per "Actors Over Locks"): a single task owns the writer,
//! the id counter, and the pending map; a reader task decodes stdout and feeds
//! messages in. No `Arc<Mutex<HashMap>>` for the request map.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::backend::subprocess::{AsyncCommandGroup, ProcessOwner, spawn};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::lsp_codec::{LspDecoder, encode};

/// Upper bound for a single LSP request. A language server that accepts a
/// request but never answers must not keep a pending-map entry and a provider
/// task alive indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const INITIALIZE_REQUEST_TIMEOUT: Duration = REQUEST_TIMEOUT;
const STDERR_CAPTURE_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LspErrorKind {
    JsonRpc,
    Timeout,
    Transport,
}

/// A failed LSP request: either a JSON-RPC `error` object from the server or a
/// transport failure (connection closed, client stopped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LspError {
    kind: LspErrorKind,
    pub code: Option<i64>,
    pub message: String,
    pub exit_status: Option<String>,
    pub stderr: Option<String>,
}

impl LspError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: LspErrorKind::Transport,
            code: None,
            message: message.into(),
            exit_status: None,
            stderr: None,
        }
    }

    fn transport_with_exit(message: impl Into<String>, exit: &LspServerExit) -> Self {
        Self {
            kind: LspErrorKind::Transport,
            code: None,
            message: message.into(),
            exit_status: exit.exit_status.clone(),
            stderr: exit.stderr.clone(),
        }
    }

    fn transport_from_exit(exit: &LspServerExit) -> Self {
        Self {
            kind: LspErrorKind::Transport,
            code: None,
            message: "LSP connection closed".to_owned(),
            exit_status: exit.exit_status.clone(),
            stderr: exit.stderr.clone(),
        }
    }

    fn timeout(id: i64, timeout: Duration) -> Self {
        Self {
            kind: LspErrorKind::Timeout,
            code: None,
            message: format!("LSP request {id} timed out after {timeout:?}"),
            exit_status: None,
            stderr: None,
        }
    }

    pub(crate) fn is_timeout(&self) -> bool {
        self.kind == LspErrorKind::Timeout
    }
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(f, "LSP error {code}: {}", self.message),
            None => write!(f, "LSP error: {}", self.message),
        }
    }
}

/// A server-initiated notification (no id), forwarded to the provider.
#[derive(Debug, Clone)]
pub(crate) struct LspNotification {
    pub method: String,
    pub params: Value,
}

/// Events delivered to the provider out-of-band from request/response: server
/// notifications, and malformed-traffic protocol errors (so the provider can
/// surface `code_intel_error` + `Failed` instead of the client silently
/// logging and continuing).
#[derive(Debug, Clone)]
pub(crate) enum LspEvent {
    Notification(LspNotification),
    /// The codec rejected bytes from the server (bad framing, oversize message,
    /// invalid JSON). Carries a human-readable description.
    ProtocolError(String),
    /// The server's stdout closed. Carries the child exit status/stderr when
    /// there was a subprocess behind this client.
    ServerExited(LspServerExit),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LspServerExit {
    pub exit_status: Option<String>,
    pub stderr: Option<String>,
}

#[derive(Clone)]
struct LspProcessDiagnostics {
    owner: ProcessOwner,
    completed: watch::Receiver<Option<LspServerExit>>,
}

#[derive(Default)]
struct StderrCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StderrCapture {
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > STDERR_CAPTURE_LIMIT {
            let excess = self.bytes.len() - STDERR_CAPTURE_LIMIT;
            self.bytes.drain(..excess);
            self.truncated = true;
        }
    }

    fn captured(&self) -> Option<String> {
        let output = String::from_utf8_lossy(&self.bytes);
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut captured = trimmed.to_owned();
        if self.truncated {
            captured.insert(0, '…');
        }
        Some(captured)
    }
}

/// Commands processed by the client actor. `Incoming` and `ReaderClosed` are
/// injected by the reader task; the rest by the public API.
enum LspCommand {
    Request {
        method: String,
        params: Value,
        started: oneshot::Sender<Result<i64, LspError>>,
        reply: oneshot::Sender<Result<Value, LspError>>,
    },
    CancelRequest {
        id: i64,
    },
    Notify {
        method: String,
        params: Value,
    },
    Incoming(Value),
    ReaderClosed,
}

/// A clonable request handle (the request half of [`LspClient`]). Issuing a
/// request only needs the command channel, so this can be moved into a detached
/// task without borrowing the actor-owned client.
#[derive(Clone)]
pub(crate) struct LspRequester {
    cmd_tx: mpsc::UnboundedSender<LspCommand>,
}

impl LspRequester {
    /// Send a request and return the assigned JSON-RPC id plus its pending
    /// response. Dropping the returned pending request before it resolves sends
    /// `$/cancelRequest` and removes the request from the client actor's pending
    /// response registry.
    pub(crate) async fn start_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<LspPendingRequest, LspError> {
        let timeout = request_timeout(method);
        let (started, assigned) = oneshot::channel();
        let (reply, response) = oneshot::channel();
        self.cmd_tx
            .send(LspCommand::Request {
                method: method.to_owned(),
                params,
                started,
                reply,
            })
            .map_err(|_| LspError::transport("LSP client stopped"))?;
        let id = assigned
            .await
            .map_err(|_| LspError::transport("LSP client dropped before issuing request"))??;
        Ok(LspPendingRequest {
            id,
            cmd_tx: self.cmd_tx.clone(),
            response: Some(response),
            done: false,
            timeout,
        })
    }

    /// Send a request and await its correlated response.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.start_request(method, params).await?.response().await
    }

    /// Send `$/cancelRequest` for an already-issued request id. The client actor
    /// removes the pending response entry before writing the notification so a
    /// late server response cannot leave the registry dangling.
    pub(crate) fn cancel_request(&self, id: i64) -> Result<(), LspError> {
        self.cmd_tx
            .send(LspCommand::CancelRequest { id })
            .map_err(|_| LspError::transport("LSP client stopped"))
    }
}

/// An issued LSP request whose JSON-RPC id is known. If the pending response is
/// dropped before it resolves (for example because a model-resolution task was
/// aborted on unsubscribe), `Drop` proactively sends `$/cancelRequest`.
pub(crate) struct LspPendingRequest {
    id: i64,
    cmd_tx: mpsc::UnboundedSender<LspCommand>,
    response: Option<oneshot::Receiver<Result<Value, LspError>>>,
    done: bool,
    timeout: Duration,
}

impl LspPendingRequest {
    pub(crate) fn id(&self) -> i64 {
        self.id
    }

    pub(crate) async fn response(mut self) -> Result<Value, LspError> {
        let response = self
            .response
            .take()
            .ok_or_else(|| LspError::transport("LSP request response already consumed"))?;
        match tokio::time::timeout(self.timeout, response).await {
            Ok(result) => {
                self.done = true;
                result.map_err(|_| LspError::transport("LSP client dropped before responding"))?
            }
            Err(_) => {
                let _ = self.cmd_tx.send(LspCommand::CancelRequest { id: self.id });
                self.done = true;
                Err(LspError::timeout(self.id, self.timeout))
            }
        }
    }
}

fn request_timeout(method: &str) -> Duration {
    match method {
        "initialize" => INITIALIZE_REQUEST_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    }
}

impl Drop for LspPendingRequest {
    fn drop(&mut self) {
        if !self.done && self.response.is_some() {
            let _ = self.cmd_tx.send(LspCommand::CancelRequest { id: self.id });
        }
    }
}

pub(crate) struct LspClient {
    cmd_tx: mpsc::UnboundedSender<LspCommand>,
    process: LspProcessDiagnostics,
    reader_task: JoinHandle<()>,
    actor_task: JoinHandle<()>,
}

impl LspClient {
    /// Spawn `binary args…` as a grouped child in `cwd`, wiring its stdio into
    /// an LSP client using the workspace's canonical execution environment.
    pub(crate) async fn spawn(
        binary: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<LspEvent>), String> {
        crate::workbench::prepare_backend_execution(None).await?;
        let mut cmd = crate::backend::subprocess::command(binary)?;
        cmd.args(args)
            .current_dir(cwd)
            .stdin(crate::backend::subprocess::ProcessStdio::Piped)
            .stdout(crate::backend::subprocess::ProcessStdio::Piped)
            .stderr(crate::backend::subprocess::ProcessStdio::Piped);
        let owner = ProcessOwner::child();
        let mut child = owner
            .scope(cmd.group_spawn())
            .await
            .map_err(|e| format!("failed to spawn language server {binary:?}: {e}"))?;

        let stdin = child.take_stdin().ok_or("failed to capture stdin")?;
        let stdout = child.take_stdout().ok_or("failed to capture stdout")?;
        let stderr = child.take_stderr().ok_or("failed to capture stderr")?;

        let stderr_task = spawn(async move {
            let mut captured = StderrCapture::default();
            let mut stderr = stderr;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        tracing::debug!(
                            target: "code_intel::lsp",
                            "language server stderr: {}",
                            String::from_utf8_lossy(&chunk[..n]).trim_end()
                        );
                        captured.push_bytes(&chunk[..n]);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "language server stderr read failed");
                        break;
                    }
                }
            }
            captured.captured()
        });
        let (completed_tx, completed) = watch::channel(None);
        spawn(async move {
            let exit_status = match child.wait().await {
                Ok(status) => Some(status.to_string()),
                Err(error) => Some(format!("failed to join language server: {error}")),
            };
            let stderr = match stderr_task.await {
                Ok(stderr) => stderr,
                Err(error) => Some(format!("failed to join language server stderr: {error}")),
            };
            completed_tx.send_replace(Some(LspServerExit {
                exit_status,
                stderr,
            }));
        });
        let process = LspProcessDiagnostics { owner, completed };
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<LspCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<LspEvent>();
        let reader_task = spawn(read_loop(stdout, cmd_tx.clone(), event_tx.clone()));
        let actor_task = spawn(actor_loop(stdin, cmd_rx, event_tx, Some(process.clone())));
        Ok((
            Self {
                cmd_tx,
                process,
                reader_task,
                actor_task,
            },
            event_rx,
        ))
    }

    /// Send a request and await its correlated response.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.requester().request(method, params).await
    }

    /// A cheap, clonable handle that can issue requests without owning the
    /// client. The provider actor holds the [`LspClient`] but spawns the
    /// definition/hover round-trip on a detached task (so a slow language
    /// server never stalls the actor loop) — that task carries an
    /// [`LspRequester`] cloned from here, not the client itself.
    pub(crate) fn requester(&self) -> LspRequester {
        LspRequester {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Fire a client→server notification (no response).
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.cmd_tx
            .send(LspCommand::Notify {
                method: method.to_owned(),
                params,
            })
            .map_err(|_| "LSP client stopped".to_owned())
    }

    /// Disabling code intelligence must release Cargo locks, including those
    /// held by descendants, without waiting for a busy server's LSP response.
    pub(crate) async fn terminate(self) {
        self.process.owner.kill_all();
        collect_server_exit(Some(&self.process)).await;
    }

    pub(crate) async fn shutdown(self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null);
        wait_server_exit(&self.process).await;
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.process.owner.kill_all();
        self.reader_task.abort();
        self.actor_task.abort();
    }
}

/// Read stdout, decode `Content-Length` frames, forward each message to the
/// actor. On EOF or error, signal `ReaderClosed` so pending requests fail
/// visibly instead of hanging forever.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    cmd_tx: mpsc::UnboundedSender<LspCommand>,
    event_tx: mpsc::UnboundedSender<LspEvent>,
) {
    let mut decoder = LspDecoder::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                decoder.extend(&chunk[..n]);
                loop {
                    match decoder.next() {
                        Ok(Some(value)) => {
                            if cmd_tx.send(LspCommand::Incoming(value)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            // The codec advanced past the bad frame, so we keep
                            // reading — but surface the malformed traffic to the
                            // provider so it can fail visibly, not just log.
                            tracing::warn!(%error, "malformed LSP message from language server");
                            let _ = event_tx.send(LspEvent::ProtocolError(error.to_string()));
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "LSP stdout read error");
                break;
            }
        }
    }
    let _ = cmd_tx.send(LspCommand::ReaderClosed);
}

/// The client actor: owns the writer, id counter, and pending map.
async fn actor_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut cmd_rx: mpsc::UnboundedReceiver<LspCommand>,
    event_tx: mpsc::UnboundedSender<LspEvent>,
    process: Option<LspProcessDiagnostics>,
) {
    let mut next_id: i64 = 1;
    let mut pending: HashMap<i64, oneshot::Sender<Result<Value, LspError>>> = HashMap::new();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            LspCommand::Request {
                method,
                params,
                started,
                reply,
            } => {
                let id = next_id;
                next_id += 1;
                let message = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                });
                match write_message(&mut writer, &message).await {
                    Ok(()) => {
                        pending.insert(id, reply);
                        let _ = started.send(Ok(id));
                    }
                    Err(error) => {
                        let error = fail_after_write_error(
                            error,
                            process.as_ref(),
                            &mut pending,
                            &event_tx,
                        )
                        .await;
                        let _ = started.send(Err(error.clone()));
                        let _ = reply.send(Err(error));
                        break;
                    }
                }
            }
            LspCommand::CancelRequest { id } => {
                let Some(reply) = pending.remove(&id) else {
                    continue;
                };
                let message = json!({
                    "jsonrpc": "2.0",
                    "method": "$/cancelRequest",
                    "params": { "id": id },
                });
                if let Err(error) = write_message(&mut writer, &message).await {
                    tracing::warn!(%error, id, "failed to write LSP cancel notification");
                    let error =
                        fail_after_write_error(error, process.as_ref(), &mut pending, &event_tx)
                            .await;
                    let _ = reply.send(Err(error));
                    break;
                } else {
                    let _ = reply.send(Err(LspError::transport(format!(
                        "LSP request {id} cancelled"
                    ))));
                }
            }
            LspCommand::Notify { method, params } => {
                let message = json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params,
                });
                if let Err(error) = write_message(&mut writer, &message).await {
                    tracing::warn!(%error, %method, "failed to write LSP notification");
                    let _ =
                        fail_after_write_error(error, process.as_ref(), &mut pending, &event_tx)
                            .await;
                    break;
                }
            }
            LspCommand::Incoming(value) => {
                if !handle_incoming(
                    value,
                    &mut writer,
                    &mut pending,
                    &event_tx,
                    process.as_ref(),
                )
                .await
                {
                    break;
                }
            }
            LspCommand::ReaderClosed => {
                let exit = collect_server_exit(process.as_ref()).await;
                let error = LspError::transport_from_exit(&exit);
                for (_, reply) in pending.drain() {
                    let _ = reply.send(Err(error.clone()));
                }
                let _ = event_tx.send(LspEvent::ServerExited(exit));
                break;
            }
        }
    }

    // Final sweep: fail anything still outstanding so callers never hang.
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(LspError::transport("LSP client stopped")));
    }
}

async fn fail_after_write_error(
    write_error: String,
    process: Option<&LspProcessDiagnostics>,
    pending: &mut HashMap<i64, oneshot::Sender<Result<Value, LspError>>>,
    event_tx: &mpsc::UnboundedSender<LspEvent>,
) -> LspError {
    let exit = collect_server_exit(process).await;
    let error = LspError::transport_with_exit(write_error, &exit);
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(error.clone()));
    }
    let _ = event_tx.send(LspEvent::ServerExited(exit));
    error
}

async fn collect_server_exit(process: Option<&LspProcessDiagnostics>) -> LspServerExit {
    match process {
        Some(process) => {
            process.owner.kill_all();
            wait_server_exit(process).await
        }
        None => LspServerExit::default(),
    }
}

async fn wait_server_exit(process: &LspProcessDiagnostics) -> LspServerExit {
    let mut completed = process.completed.clone();
    loop {
        if let Some(exit) = completed.borrow_and_update().clone() {
            return exit;
        }
        if completed.changed().await.is_err() {
            return LspServerExit {
                exit_status: Some(
                    "Language server owner stopped without a completion event".to_owned(),
                ),
                stderr: None,
            };
        }
    }
}

/// Dispatch one decoded incoming message: a response to a pending request, a
/// server→client request (auto-answered), or a notification (forwarded).
async fn handle_incoming<W: AsyncWrite + Unpin>(
    value: Value,
    writer: &mut W,
    pending: &mut HashMap<i64, oneshot::Sender<Result<Value, LspError>>>,
    event_tx: &mpsc::UnboundedSender<LspEvent>,
    process: Option<&LspProcessDiagnostics>,
) -> bool {
    // A `method` field means request (with id) or notification (without id).
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let method = method.to_owned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        match value.get("id") {
            Some(id) => {
                // Server→client request: answer with a conservative default so
                // rust-analyzer doesn't block. Echo the id verbatim.
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": auto_response_result(&method, &params),
                });
                if let Err(error) = write_message(writer, &response).await {
                    tracing::warn!(%error, %method, "failed to answer server-initiated request");
                    let _ = fail_after_write_error(error, process, pending, event_tx).await;
                    return false;
                }
            }
            None => {
                let _ = event_tx.send(LspEvent::Notification(LspNotification { method, params }));
            }
        }
        return true;
    }

    // Otherwise it's a response to one of our requests.
    let Some(id) = value.get("id").and_then(Value::as_i64) else {
        tracing::warn!(
            ?value,
            "unrecognized LSP message (no method, no numeric id)"
        );
        return true;
    };
    let Some(reply) = pending.remove(&id) else {
        tracing::debug!(id, "LSP response for unknown/expired request id");
        return true;
    };
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("LSP error")
            .to_owned();
        let _ = reply.send(Err(LspError {
            kind: LspErrorKind::JsonRpc,
            code,
            message,
            exit_status: None,
            stderr: None,
        }));
    } else {
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        let _ = reply.send(Ok(result));
    }
    true
}

/// Default result for an auto-answered server→client request. `workspace/
/// configuration` must return one entry per requested item; everything else
/// gets `null`, which the LSP spec accepts for the requests rust-analyzer makes
/// during startup (`window/workDoneProgress/create`, `client/registerCapability`).
fn auto_response_result(method: &str, params: &Value) -> Value {
    match method {
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            Value::Array(vec![Value::Null; count])
        }
        _ => Value::Null,
    }
}

async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Value,
) -> Result<(), String> {
    let framed = encode(message);
    writer
        .write_all(&framed)
        .await
        .map_err(|e| format!("write failed: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("flush failed: {e}"))
}
