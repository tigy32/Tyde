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
use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::lsp_codec::{LspDecoder, encode};
use crate::backend::subprocess::reap_group_child_slot;

/// Upper bound for a single LSP request. A language server that accepts a
/// request but never answers must not keep a pending-map entry and a provider
/// task alive indefinitely.
#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const INITIALIZE_REQUEST_TIMEOUT: Duration = REQUEST_TIMEOUT;
#[cfg(test)]
const INITIALIZE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_EXIT_WAIT: Duration = Duration::from_millis(250);
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

struct LspProcessDiagnostics {
    child: Arc<Mutex<Option<AsyncGroupChild>>>,
    stderr: Arc<Mutex<StderrCapture>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
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
    #[cfg(test)]
    PendingCount {
        reply: oneshot::Sender<usize>,
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
    /// The owned child slot, shared with the reaper. `None` for the in-memory
    /// (`from_io`) constructor used in tests.
    child: Option<Arc<Mutex<Option<AsyncGroupChild>>>>,
    reader_task: JoinHandle<()>,
    actor_task: JoinHandle<()>,
}

impl LspClient {
    /// Spawn `binary args…` as a grouped child in `cwd`, wiring its stdio into
    /// an LSP client. `env_path` overrides the child's `PATH` (so rust-analyzer
    /// can find `cargo`/`rustc` — pass `process_env::resolved_child_process_path()`).
    pub(crate) async fn spawn(
        binary: &Path,
        args: &[String],
        cwd: &Path,
        env_path: Option<&OsStr>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<LspEvent>), String> {
        let mut cmd = Command::new(binary);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = env_path {
            cmd.env("PATH", path);
        }
        let mut child = cmd
            .group_spawn()
            .map_err(|e| format!("failed to spawn language server {binary:?}: {e}"))?;

        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or("failed to capture stdin")?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or("failed to capture stdout")?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or("failed to capture stderr")?;

        let stderr_capture = Arc::new(Mutex::new(StderrCapture::default()));
        let stderr_capture_for_task = stderr_capture.clone();
        // Drain stderr to the log and retain the tail so a crashed server's
        // real failure reason can be surfaced in the typed error payload.
        let stderr_task = tokio::spawn(async move {
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
                        stderr_capture_for_task.lock().await.push_bytes(&chunk[..n]);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "language server stderr read error");
                        break;
                    }
                }
            }
        });

        let child_slot = Arc::new(Mutex::new(Some(child)));
        let process = LspProcessDiagnostics {
            child: child_slot.clone(),
            stderr: stderr_capture,
            stderr_task: Mutex::new(Some(stderr_task)),
        };
        let (client, notifications) =
            Self::from_io_inner(stdin, stdout, Some(child_slot), Some(process));
        Ok((client, notifications))
    }

    /// I/O-agnostic core. Used by [`spawn`](Self::spawn) with real child stdio
    /// and by tests with in-memory `tokio::io::duplex` pipes (a fake LSP
    /// server on the other end). `child` is `Some` only when there is a
    /// subprocess to reap.
    #[cfg(test)]
    pub(crate) fn from_io<W, R>(
        writer: W,
        reader: R,
        child: Option<Arc<Mutex<Option<AsyncGroupChild>>>>,
    ) -> (Self, mpsc::UnboundedReceiver<LspEvent>)
    where
        W: AsyncWrite + Unpin + Send + 'static,
        R: AsyncRead + Unpin + Send + 'static,
    {
        Self::from_io_inner(writer, reader, child, None)
    }

    fn from_io_inner<W, R>(
        writer: W,
        reader: R,
        child: Option<Arc<Mutex<Option<AsyncGroupChild>>>>,
        process: Option<LspProcessDiagnostics>,
    ) -> (Self, mpsc::UnboundedReceiver<LspEvent>)
    where
        W: AsyncWrite + Unpin + Send + 'static,
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<LspCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<LspEvent>();

        let reader_cmd_tx = cmd_tx.clone();
        let reader_task = tokio::spawn(read_loop(reader, reader_cmd_tx, event_tx.clone()));
        let actor_task = tokio::spawn(actor_loop(writer, cmd_rx, event_tx, process));

        (
            Self {
                cmd_tx,
                child,
                reader_task,
                actor_task,
            },
            event_rx,
        )
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

    #[cfg(test)]
    async fn pending_request_count(&self) -> usize {
        let (reply, response) = oneshot::channel();
        let _ = self.cmd_tx.send(LspCommand::PendingCount { reply });
        response.await.unwrap_or(0)
    }

    /// Graceful LSP teardown: `shutdown` request (bounded), `exit`
    /// notification, then kill+reap the process group. Bounded everywhere so a
    /// wedged server can never hang teardown.
    pub(crate) async fn shutdown(self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.request("shutdown", Value::Null),
        )
        .await;
        let _ = self.notify("exit", Value::Null);
        if let Some(child) = &self.child {
            let mut guard = child.lock().await;
            if let Some(mut c) = guard.take() {
                match tokio::time::timeout(Duration::from_secs(2), c.wait()).await {
                    Ok(_) => {}
                    Err(_) => {
                        let _ = c.kill().await;
                    }
                }
            }
        }
        // Drop aborts the reader/actor tasks; the child slot is already empty.
    }

    /// Test/diagnostic accessor: a clone of the owned child slot. Lets a
    /// teardown test observe that the reaper emptied the slot (no leaked child)
    /// after the client is dropped.
    #[cfg(test)]
    pub(crate) fn child_slot(&self) -> Option<Arc<Mutex<Option<AsyncGroupChild>>>> {
        self.child.clone()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.actor_task.abort();
        if let Some(child) = &self.child {
            reap_group_child_slot(child);
        }
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
            #[cfg(test)]
            LspCommand::PendingCount { reply } => {
                let _ = reply.send(pending.len());
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
    let Some(process) = process else {
        return LspServerExit::default();
    };

    let exit_status = collect_exit_status(process).await;
    await_stderr_task(process).await;
    let stderr = process.stderr.lock().await.captured();
    LspServerExit {
        exit_status,
        stderr,
    }
}

async fn collect_exit_status(process: &LspProcessDiagnostics) -> Option<String> {
    let deadline = tokio::time::Instant::now() + SERVER_EXIT_WAIT;
    loop {
        let status = {
            let mut guard = process.child.lock().await;
            match guard.as_mut() {
                Some(child) => child.inner().try_wait(),
                None => return None,
            }
        };
        match status {
            Ok(Some(status)) => return Some(status.to_string()),
            Ok(None) if tokio::time::Instant::now() >= deadline => return None,
            Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(error) => return Some(format!("failed to query process status: {error}")),
        }
    }
}

async fn await_stderr_task(process: &LspProcessDiagnostics) {
    let Some(mut task) = process.stderr_task.lock().await.take() else {
        return;
    };
    tokio::select! {
        _ = &mut task => {}
        _ = tokio::time::sleep(SERVER_EXIT_WAIT) => {
            task.abort();
            let _ = task.await;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal fake LSP server over an in-memory pipe pair. It echoes each
    /// request's id back with a result that names the method, and — to prove id
    /// correlation rather than ordering — buffers the first two requests and
    /// answers them in **reverse** order. It also emits one unsolicited
    /// `publishDiagnostics` notification.
    async fn fake_server<R, W>(reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = reader;
        let mut decoder = LspDecoder::new();
        let mut chunk = [0u8; 4096];
        let mut buffered: Vec<Value> = Vec::new();
        let mut sent_notification = false;

        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            decoder.extend(&chunk[..n]);
            while let Ok(Some(msg)) = decoder.next() {
                // Ignore notifications from the client (e.g. `exit`).
                if msg.get("id").is_none() {
                    continue;
                }
                buffered.push(msg);

                if !sent_notification {
                    sent_notification = true;
                    let note = json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {"uri": "file:///x.rs", "diagnostics": []},
                    });
                    let _ = write_message(&mut writer, &note).await;
                }

                // Answer in reverse once we have two requests buffered.
                if buffered.len() == 2 {
                    for req in buffered.drain(..).rev() {
                        let id = req.get("id").cloned().unwrap();
                        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"method": method},
                        });
                        let _ = write_message(&mut writer, &response).await;
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn correlates_responses_and_delivers_notifications() {
        // Two duplex pairs: client→server and server→client.
        let (client_to_server_w, client_to_server_r) = tokio::io::duplex(64 * 1024);
        let (server_to_client_w, server_to_client_r) = tokio::io::duplex(64 * 1024);

        tokio::spawn(fake_server(client_to_server_r, server_to_client_w));

        let (client, mut notifications) =
            LspClient::from_io(client_to_server_w, server_to_client_r, None);

        // Fire two requests concurrently; the server answers them in reverse,
        // so a correct client must correlate by id, not by arrival order.
        let alpha = client.request("alpha", json!({}));
        let beta = client.request("beta", json!({}));
        let (alpha, beta) = tokio::join!(alpha, beta);

        assert_eq!(alpha.unwrap(), json!({"method": "alpha"}));
        assert_eq!(beta.unwrap(), json!({"method": "beta"}));

        let event = notifications.recv().await.expect("an event arrived");
        match event {
            LspEvent::Notification(note) => {
                assert_eq!(note.method, "textDocument/publishDiagnostics");
            }
            LspEvent::ProtocolError(error) => panic!("unexpected protocol error: {error}"),
            LspEvent::ServerExited(exit) => panic!("unexpected server exit: {exit:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_traffic_surfaces_as_protocol_error_event() {
        let (client_to_server_w, client_to_server_r) = tokio::io::duplex(64 * 1024);
        let (mut server_to_client_w, server_to_client_r) = tokio::io::duplex(64 * 1024);

        // Server that drains the client and emits a header with garbage
        // Content-Length — the codec must reject it and the client must surface
        // a ProtocolError event.
        tokio::spawn(async move {
            let mut reader = client_to_server_r;
            let mut buf = [0u8; 256];
            let _ = reader.read(&mut buf).await;
            let _ = server_to_client_w
                .write_all(b"Content-Length: not-a-number\r\n\r\n{}")
                .await;
            let _ = server_to_client_w.flush().await;
            // Keep the write side open so the client doesn't see EOF first.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let (client, mut events) = LspClient::from_io(client_to_server_w, server_to_client_r, None);
        // Nudge the server to respond.
        let _ = client.notify("ping", json!({}));

        let event = events.recv().await.expect("an event arrived");
        assert!(
            matches!(event, LspEvent::ProtocolError(_)),
            "malformed traffic should surface as a ProtocolError event, got {event:?}"
        );
    }

    #[tokio::test]
    async fn pending_requests_fail_when_server_disconnects() {
        let (client_to_server_w, client_to_server_r) = tokio::io::duplex(1024);
        let (server_to_client_w, server_to_client_r) = tokio::io::duplex(1024);

        // Server that reads nothing and immediately closes its write side.
        drop(server_to_client_w);
        tokio::spawn(async move {
            let mut r = client_to_server_r;
            let mut buf = [0u8; 64];
            let _ = r.read(&mut buf).await;
        });

        let (client, _notes) = LspClient::from_io(client_to_server_w, server_to_client_r, None);
        let result = client.request("initialize", json!({})).await;
        assert!(result.is_err(), "request must fail when stdout is closed");
    }

    #[tokio::test]
    async fn unanswered_request_times_out_and_cleans_pending_entry() {
        let (client_to_server_w, mut client_to_server_r) = tokio::io::duplex(64 * 1024);
        let (server_to_client_w, server_to_client_r) = tokio::io::duplex(64 * 1024);
        let captured = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let captured_for_server = captured.clone();

        tokio::spawn(async move {
            let _writer = server_to_client_w;
            let mut decoder = LspDecoder::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = match client_to_server_r.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.extend(&chunk[..n]);
                while let Ok(Some(msg)) = decoder.next() {
                    captured_for_server
                        .lock()
                        .expect("captured mutex poisoned")
                        .push(msg);
                }
            }
        });

        let (client, _notes) = LspClient::from_io(client_to_server_w, server_to_client_r, None);
        let method = "textDocument/hover";
        let result = client.request(method, json!({})).await;
        let error = result.expect_err("silent server should time out the request");
        assert!(error.is_timeout(), "expected timeout error, got {error}");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if client.pending_request_count().await == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed-out request stayed in pending map"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let messages = captured.lock().expect("captured mutex poisoned");
        let request_id = messages
            .iter()
            .find(|msg| msg.get("method").and_then(Value::as_str) == Some(method))
            .and_then(|msg| msg.get("id"))
            .and_then(Value::as_i64)
            .expect("hover request was sent with an id");
        let cancelled_id = messages
            .iter()
            .find(|msg| msg.get("method").and_then(Value::as_str) == Some("$/cancelRequest"))
            .and_then(|msg| msg.get("params"))
            .and_then(|params| params.get("id"))
            .and_then(Value::as_i64);
        assert_eq!(
            cancelled_id,
            Some(request_id),
            "timeout should cancel and reclaim the in-flight request id"
        );
    }

    #[tokio::test]
    async fn teardown_reaps_child_no_zombie() {
        // Reuse the real group-spawn + reap path with a cheap long-lived
        // process. After the client drops, the reaper must empty the child
        // slot — proving the child handle isn't leaked.
        let Some(sleep_bin) = crate::process_env::find_executable_in_path("sleep") else {
            eprintln!("SKIP teardown_reaps_child_no_zombie: `sleep` not found on PATH");
            return;
        };

        let cwd = std::env::temp_dir();
        let (client, _notes) = LspClient::spawn(&sleep_bin, &["30".to_owned()], &cwd, None)
            .await
            .expect("spawn sleep");

        let slot = client.child_slot().expect("real child has a slot");
        {
            let mut guard = slot.lock().await;
            let child = guard.as_mut().expect("child present after spawn");
            assert!(
                matches!(child.try_wait(), Ok(None)),
                "child should be alive right after spawn"
            );
        }

        drop(client);

        // The detached reaper kills the group and empties the slot. Poll a
        // bounded number of times so a CI hiccup doesn't make this flaky.
        let mut reaped = false;
        for _ in 0..50 {
            if slot.lock().await.is_none() {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(reaped, "child slot was not reaped after drop — zombie/leak");
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .is_ok_and(|status| status.success())
    }

    #[tokio::test]
    async fn exited_child_surfaces_late_stderr_and_status() {
        let Some(shell) = crate::process_env::find_executable_in_path("sh") else {
            eprintln!("SKIP exited_child_surfaces_late_stderr_and_status: `sh` not found on PATH");
            return;
        };
        let cwd = tempfile::tempdir().expect("temp dir");
        let (client, mut events) = LspClient::spawn(
            &shell,
            &[
                "-c".to_owned(),
                "exec 1>&-; sleep 0.05; printf 'fatal language-server startup' >&2; exit 7"
                    .to_owned(),
            ],
            cwd.path(),
            None,
        )
        .await
        .expect("spawn shell");

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("server exit event within timeout")
            .expect("event channel open");
        match event {
            LspEvent::ServerExited(exit) => {
                assert!(
                    exit.exit_status
                        .as_deref()
                        .is_some_and(|status| status.contains('7')),
                    "exit status should name the child failure, got {exit:?}"
                );
                assert_eq!(
                    exit.stderr.as_deref(),
                    Some("fatal language-server startup")
                );
            }
            other => panic!("expected ServerExited, got {other:?}"),
        }
        drop(client);
    }

    #[tokio::test]
    async fn stderr_tail_is_bounded_for_newline_less_output() {
        let Some(shell) = crate::process_env::find_executable_in_path("sh") else {
            eprintln!(
                "SKIP stderr_tail_is_bounded_for_newline_less_output: `sh` not found on PATH"
            );
            return;
        };
        let cwd = tempfile::tempdir().expect("temp dir");
        let giant = "x".repeat(STDERR_CAPTURE_LIMIT + 4096);
        let script = format!("printf '{giant}' >&2; exit 9");
        let (client, mut events) =
            LspClient::spawn(&shell, &["-c".to_owned(), script], cwd.path(), None)
                .await
                .expect("spawn shell");

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("server exit event within timeout")
            .expect("event channel open");
        match event {
            LspEvent::ServerExited(exit) => {
                let stderr = exit.stderr.expect("bounded stderr captured");
                assert!(
                    stderr.len() <= STDERR_CAPTURE_LIMIT + "…".len(),
                    "stderr capture should retain only the bounded tail, len={}",
                    stderr.len()
                );
                assert!(
                    stderr.starts_with('…'),
                    "truncated stderr should be marked as a tail"
                );
                assert!(
                    stderr.chars().skip(1).all(|ch| ch == 'x'),
                    "stderr tail should contain the fake server's output"
                );
            }
            other => panic!("expected ServerExited, got {other:?}"),
        }
        drop(client);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_after_server_exit_reaps_lingering_process_group_child() {
        let Some(shell) = crate::process_env::find_executable_in_path("sh") else {
            eprintln!(
                "SKIP drop_after_server_exit_reaps_lingering_process_group_child: `sh` not found on PATH"
            );
            return;
        };
        let Some(sleep_bin) = crate::process_env::find_executable_in_path("sleep") else {
            eprintln!(
                "SKIP drop_after_server_exit_reaps_lingering_process_group_child: `sleep` not found on PATH"
            );
            return;
        };
        let cwd = tempfile::tempdir().expect("temp dir");
        let pid_file = cwd.path().join("grandchild.pid");
        let script = format!(
            "('{}' 30) >/dev/null 2>/dev/null & echo $! > '{}'; printf 'server crashed' >&2; exit 7",
            sleep_bin.display(),
            pid_file.display()
        );
        let (client, mut events) =
            LspClient::spawn(&shell, &["-c".to_owned(), script], cwd.path(), None)
                .await
                .expect("spawn shell");
        let slot = client.child_slot().expect("real child has a slot");

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("server exit event within timeout")
            .expect("event channel open");
        assert!(
            matches!(event, LspEvent::ServerExited(_)),
            "expected ServerExited, got {event:?}"
        );

        let pid = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&pid_file).await {
                    let pid = contents.trim().parse::<u32>().expect("child pid");
                    break pid;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "fake server did not write child pid"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        assert!(
            process_exists(pid),
            "fake server's child should still be alive before drop"
        );

        drop(client);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if slot.lock().await.is_none() && !process_exists(pid) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "process-group reaper did not kill and reap lingering child {pid}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
