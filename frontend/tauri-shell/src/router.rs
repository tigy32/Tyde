use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::bridge::{
    HOST_DISCONNECTED_EVENT, HOST_ERROR_EVENT, HOST_LINE_EVENT, HOST_WARNING_EVENT,
    HostDisconnectedEvent, HostErrorEvent, HostLineEvent, HostWarningEvent,
};
use crate::host_store::{HostTransportConfig, RemoteHostLifecycleConfig};
use crate::remote_bootstrap::{current_app_release_version, shell_quote};
use host_config::TydeReleaseVersion;

const DEFAULT_REMOTE_HOST_COMMAND: &str = "tyde host --bridge-uds";
const SSH_STDERR_CAPTURE_LIMIT: usize = 64 * 1024;
const SSH_EXIT_WAIT: Duration = Duration::from_secs(2);
pub const HOST_VOICE_FRAME_EVENT: &str = "tyde://host-voice-frame";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HostVoiceFrameEvent {
    host_id: String,
    envelope: String,
    opus: Vec<u8>,
}

#[derive(Debug)]
enum Outbound {
    Line(String),
    Frame(protocol::ProtocolFrame, Option<AudioPermit>),
}
#[derive(Debug)]
struct AudioPermit(Arc<AtomicUsize>);
impl Drop for AudioPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Routes Tauri commands to per-connection writer tasks and tracks the live
/// connections.
///
/// Each connection is driven by two fully independent tasks that share no
/// channel and never coordinate across directions:
///
///   * a **reader task** that solely owns the transport's read half and emits
///     every inbound line straight to the app, and
///   * a **writer task** that solely owns the write half and drains an
///     unbounded outbound channel.
///
/// The registry below only carries control-plane state (the outbound sender
/// and the child handle used for teardown). It never sits in the inbound data
/// path, so a stalled/backpressured writer can never stop the reader from
/// draining the transport — which is what previously deadlocked the SSH proxy.
#[derive(Clone)]
pub struct ProxyRouterHandle {
    state: Arc<Mutex<RouterState>>,
}

struct RouterState {
    hosts: HashMap<String, Connection>,
    next_connection_id: u64,
}

struct Connection {
    connection_id: u64,
    /// Outbound frames are enqueued here; the writer task owns the receiver.
    /// Dropping every sender (i.e. removing this entry) makes the writer task
    /// finish on its own.
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    /// SSH child process, owned here so teardown can reap it. `None` for the
    /// in-process embedded transport.
    child: Option<SshChild>,
    /// Liveness flag shared with this connection's reader/stderr tasks. Cleared
    /// (non-blocking, no lock) the instant the entry is removed or superseded,
    /// so a stale reader/stderr task that is still draining stops emitting
    /// instead of leaking late frames/errors into a newer connection that
    /// reused the same host id.
    live: Arc<AtomicBool>,
    audio_pending: Arc<AtomicUsize>,
}

struct SshChild {
    process: Child,
    stderr_capture: Arc<Mutex<StderrCapture>>,
    stderr_task: JoinHandle<Result<(), String>>,
}

#[derive(Default)]
struct StderrCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StderrCapture {
    fn push(&mut self, chunk: &[u8]) {
        let remaining = SSH_STDERR_CAPTURE_LIMIT.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        self.truncated |= chunk.len() > remaining;
    }

    fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).trim().to_owned();
        if self.truncated {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[SSH diagnostic truncated]");
        }
        text
    }
}

#[derive(Clone, Copy)]
enum CloseCause {
    NaturalEof,
    Abort,
}

impl ProxyRouterHandle {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RouterState {
                hosts: HashMap::new(),
                next_connection_id: 1,
            })),
        }
    }

    pub async fn connect_local(
        &self,
        app: AppHandle,
        host_id: String,
        transport: HostTransportConfig,
        host: server::HostHandle,
    ) -> Result<(), String> {
        tracing::info!(host_id, "connect_duplex requested");

        // Quietly tear down any existing connection for this host before
        // establishing the new one. Removing the entry drops its outbound
        // sender (stopping the old writer); reaping the child closes the old
        // reader via EOF. The old reader's teardown will no-op because the new
        // connection carries a different connection id.
        let existing = {
            let mut guard = self.state.lock().expect("router state poisoned");
            guard.hosts.remove(&host_id)
        };
        if let Some(existing) = existing {
            tracing::info!(host_id, "replacing existing host connection");
            existing.live.store(false, Ordering::Relaxed);
            #[cfg(not(target_os = "windows"))]
            if let Err(error) = app
                .state::<crate::ShellState>()
                .voice_media
                .stop_for_host(&host_id)
            {
                let _ = emit_error(&app, &host_id, error);
            }
            abort_child(existing.child).await;
        }

        let connection_id = {
            let mut guard = self.state.lock().expect("router state poisoned");
            let id = guard.next_connection_id;
            guard.next_connection_id += 1;
            id
        };

        let live = Arc::new(AtomicBool::new(true));
        let setup =
            setup_connection_transport(&host_id, app.clone(), transport, host, live.clone())
                .await?;

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Outbound>();
        let audio_pending = Arc::new(AtomicUsize::new(0));

        // Register before spawning so that an immediate reader EOF can find the
        // entry (and thus reap the child / emit disconnect) instead of racing.
        {
            let mut guard = self.state.lock().expect("router state poisoned");
            guard.hosts.insert(
                host_id.clone(),
                Connection {
                    connection_id,
                    outbound_tx,
                    child: setup.child,
                    live: live.clone(),
                    audio_pending,
                },
            );
        }

        tokio::spawn(reader_task(
            self.state.clone(),
            app.clone(),
            host_id.clone(),
            connection_id,
            setup.reader,
            live.clone(),
        ));
        tokio::spawn(writer_task(
            self.state.clone(),
            app,
            host_id.clone(),
            connection_id,
            setup.writer,
            outbound_rx,
            live,
        ));

        tracing::info!(host_id, "connected via duplex");
        Ok(())
    }

    pub async fn disconnect(&self, host_id: String) -> Result<(), String> {
        let connection = {
            let mut guard = self.state.lock().expect("router state poisoned");
            guard.hosts.remove(&host_id)
        };
        let Some(connection) = connection else {
            return Err(format!("host {host_id} is not connected"));
        };

        // Quiet teardown: clearing `live` stops the reader/stderr tasks from
        // emitting any late frames, dropping `connection` drops the outbound
        // sender (the writer task ends), and reaping the child closes the
        // reader via EOF. The reader's own teardown no-ops because the entry is
        // already gone, so no `HOST_DISCONNECTED_EVENT` is emitted for an
        // explicit disconnect.
        connection.live.store(false, Ordering::Relaxed);
        abort_child(connection.child).await;
        Ok(())
    }

    pub async fn send_line(&self, host_id: String, line: String) -> Result<(), String> {
        if line.contains('\n') {
            return Err("host line must not contain a newline".to_owned());
        }

        let outbound_tx = {
            let guard = self.state.lock().expect("router state poisoned");
            guard.hosts.get(&host_id).map(|c| c.outbound_tx.clone())
        };
        let Some(outbound_tx) = outbound_tx else {
            return Err(format!("host {host_id} is not connected"));
        };

        // Enqueue and return immediately. The unbounded channel never applies
        // backpressure, so this can't block on the writer. If the writer task
        // has already exited (dead connection), the send fails and we surface
        // an explicit error rather than silently dropping the frame.
        outbound_tx
            .send(Outbound::Line(line))
            .map_err(|_| format!("host {host_id} connection is no longer available"))
    }

    pub fn send_frame(
        &self,
        host_id: String,
        envelope: protocol::Envelope,
        binary: Vec<u8>,
    ) -> Result<(), String> {
        if binary.is_empty() || envelope.kind != protocol::FrameKind::VoiceAudio {
            return Err("desktop binary IPC only accepts typed voice audio".into());
        }
        let payload: protocol::VoiceAudioPayload = envelope
            .parse_payload()
            .map_err(|_| "invalid desktop voice audio payload")?;
        payload.validate_body(binary.len()).map_err(str::to_owned)?;
        if payload.direction != protocol::VoiceDirection::Input
            || envelope.stream != protocol::StreamPath(format!("/voice/{}", payload.session_id.0))
        {
            return Err("desktop voice audio session routing mismatch".into());
        }
        let (tx, audio_pending) = self
            .state
            .lock()
            .expect("router state poisoned")
            .hosts
            .get(&host_id)
            .map(|c| (c.outbound_tx.clone(), c.audio_pending.clone()))
            .ok_or_else(|| format!("host {host_id} is not connected"))?;
        let previous = audio_pending.fetch_add(1, Ordering::AcqRel);
        if previous >= 8 {
            audio_pending.fetch_sub(1, Ordering::AcqRel);
            return Err("desktop voice audio queue is full".into());
        }
        let permit = Some(AudioPermit(audio_pending));
        tx.send(Outbound::Frame(
            protocol::ProtocolFrame { envelope, binary },
            permit,
        ))
        .map_err(|_| format!("host {host_id} connection is no longer available"))
    }
}

/// Inbound half: solely owns the transport reader and pushes every line to the
/// app. It never touches the registry on the hot path and never waits on
/// anything the writer owns, so it always drains the transport.
async fn reader_task(
    state: Arc<Mutex<RouterState>>,
    app: AppHandle,
    host_id: String,
    connection_id: u64,
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    live: Arc<AtomicBool>,
) {
    let mut reader = protocol::FrameReader::new(reader);
    let mut close_cause = CloseCause::Abort;
    loop {
        match reader.read_frame().await {
            Ok(None) => {
                close_cause = CloseCause::NaturalEof;
                break;
            }
            Ok(Some(frame)) => {
                // Non-blocking liveness check (no lock, no await): if this
                // connection was superseded/torn down while we were parked in
                // read_line, stop so we never leak a late frame into a newer
                // connection that reused this host id.
                if !live.load(Ordering::Relaxed) {
                    break;
                }
                if !frame.binary.is_empty() {
                    if frame.envelope.kind != protocol::FrameKind::VoiceAudio {
                        let _ = emit_error(
                            &app,
                            &host_id,
                            "host sent an unauthorized binary frame".into(),
                        );
                        break;
                    }
                    let payload = match frame
                        .envelope
                        .parse_payload::<protocol::VoiceAudioPayload>()
                    {
                        Ok(payload) => payload,
                        Err(_) => {
                            let _ = emit_error(
                                &app,
                                &host_id,
                                "host sent invalid voice audio metadata".into(),
                            );
                            break;
                        }
                    };
                    if payload.direction != protocol::VoiceDirection::Output {
                        let _ =
                            emit_error(&app, &host_id, "host voice audio routing mismatch".into());
                        break;
                    }
                    // Downlink audio arrives on its dedicated sub-stream —
                    // its envelope seqs are deliberately outside the JSON
                    // envelope stream's counter (the frontend validates that
                    // one and never sees these frames).
                    if payload.validate_body(frame.binary.len()).is_err()
                        || frame.envelope.stream
                            != protocol::StreamPath(format!(
                                "/voice/{}/audio",
                                payload.session_id.0
                            ))
                    {
                        let _ =
                            emit_error(&app, &host_id, "host voice audio routing mismatch".into());
                        break;
                    }
                    let envelope = match serde_json::to_string(&frame.envelope) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = emit_error(
                                &app,
                                &host_id,
                                format!("failed to encode voice frame: {error}"),
                            );
                            break;
                        }
                    };
                    let _ = app.emit(
                        HOST_VOICE_FRAME_EVENT,
                        HostVoiceFrameEvent {
                            host_id: host_id.clone(),
                            envelope,
                            opus: frame.binary,
                        },
                    );
                    continue;
                }
                let line = match serde_json::to_string(&frame.envelope) {
                    Ok(line) => line,
                    Err(error) => {
                        let _ = emit_error(
                            &app,
                            &host_id,
                            format!("failed to serialize host frame: {error}"),
                        );
                        break;
                    }
                };
                #[cfg(not(target_os = "windows"))]
                if frame.envelope.kind == protocol::FrameKind::VoiceAccepted
                    && let Ok(accepted) = frame
                        .envelope
                        .parse_payload::<protocol::VoiceAcceptedPayload>()
                    && let Err(error) = app
                        .state::<crate::ShellState>()
                        .voice_media
                        .authorize(host_id.clone(), accepted.generation)
                {
                    let _ = emit_error(&app, &host_id, error);
                }
                tracing::info!(
                    host_id,
                    connection_id,
                    line_len = line.len(),
                    "proxy router received line from host"
                );
                if let Err(error) = app.emit(
                    HOST_LINE_EVENT,
                    HostLineEvent {
                        host_id: host_id.clone(),
                        line,
                        connection_instance_id: None,
                        delivery_id: None,
                    },
                ) {
                    let _ =
                        emit_error(&app, &host_id, format!("failed to emit host line: {error}"));
                }
            }
            Err(error) => {
                if live.load(Ordering::Relaxed) {
                    let _ = emit_error(
                        &app,
                        &host_id,
                        format!("failed to read from host connection: {error}"),
                    );
                }
                break;
            }
        }
    }

    close_connection(&state, &app, &host_id, connection_id, close_cause).await;
}

/// Outbound half: solely owns the transport writer and drains the unbounded
/// command channel. It never touches the reader. When every outbound sender is
/// dropped (the connection was torn down elsewhere) `recv` returns `None` and
/// the task ends quietly.
async fn writer_task(
    state: Arc<Mutex<RouterState>>,
    app: AppHandle,
    host_id: String,
    connection_id: u64,
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    mut outbound_rx: mpsc::UnboundedReceiver<Outbound>,
    live: Arc<AtomicBool>,
) {
    while let Some(outbound) = outbound_rx.recv().await {
        let result = match outbound {
            Outbound::Line(line) => write_line(&mut writer, line).await,
            Outbound::Frame(frame, permit) => {
                let result = protocol::write_frame(&mut writer, &frame)
                    .await
                    .map_err(|e| format!("failed to write host voice frame: {e}"));
                drop(permit);
                result
            }
        };
        if let Err(error) = result {
            tracing::warn!(
                host_id,
                connection_id,
                %error,
                "closing host connection after write failed"
            );
            // Surface the write failure and tear the connection down so a dead
            // pipe becomes a visible error instead of silently swallowing sends
            // — but only if we are still the live connection, so a write that
            // failed because we were superseded doesn't raise a false error on
            // the connection that replaced us.
            if live.load(Ordering::Relaxed) {
                let _ = emit_error(&app, &host_id, error);
            }
            close_connection(&state, &app, &host_id, connection_id, CloseCause::Abort).await;
            return;
        }
    }
}

/// Drop a connection from the registry and notify the app. Idempotent and
/// connection-id guarded: only the task that still matches the live entry wins
/// the removal, so the disconnect event fires exactly once and a stale task can
/// never tear down a newer connection that reused the same host id.
async fn close_connection(
    state: &Arc<Mutex<RouterState>>,
    app: &AppHandle,
    host_id: &str,
    connection_id: u64,
    cause: CloseCause,
) {
    let connection = {
        let mut guard = state.lock().expect("router state poisoned");
        match guard.hosts.get(host_id) {
            Some(existing) if existing.connection_id == connection_id => {
                guard.hosts.remove(host_id)
            }
            _ => None,
        }
    };
    let Some(connection) = connection else {
        return;
    };

    connection.live.store(false, Ordering::Relaxed);
    #[cfg(not(target_os = "windows"))]
    if let Err(error) = app
        .state::<crate::ShellState>()
        .voice_media
        .stop_for_host(host_id)
    {
        let _ = emit_error(app, host_id, error);
    }
    if let Some(error) = finish_child(connection.child, cause).await {
        let _ = emit_error(app, host_id, error);
    }
    tracing::info!(host_id, connection_id, "connection closed");
    let _ = app.emit(
        HOST_DISCONNECTED_EVENT,
        HostDisconnectedEvent {
            host_id: host_id.to_owned(),
        },
    );
}

async fn abort_child(child: Option<SshChild>) {
    let Some(mut child) = child else {
        return;
    };
    let _ = child.process.kill().await;
    match child.process.wait().await {
        Ok(status) => {
            tracing::info!(%status, "ssh transport exited");
        }
        Err(error) => {
            tracing::warn!(%error, "failed to wait for ssh transport exit");
        }
    }
    let _ = child.stderr_task.await;
}

async fn finish_child(child: Option<SshChild>, cause: CloseCause) -> Option<String> {
    let mut child = child?;
    if matches!(cause, CloseCause::Abort) {
        abort_child(Some(child)).await;
        return None;
    }

    let status = match tokio::time::timeout(SSH_EXIT_WAIT, child.process.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            let _ = child.stderr_task.await;
            return Some(format!("failed to wait for ssh transport exit: {error}"));
        }
        Err(_) => {
            let _ = child.process.kill().await;
            let _ = child.process.wait().await;
            None
        }
    };
    let stderr_result = child.stderr_task.await;
    let diagnostic = child
        .stderr_capture
        .lock()
        .expect("ssh stderr capture poisoned")
        .text();

    match stderr_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Some(error),
        Err(error) => return Some(format!("failed to join ssh stderr capture: {error}")),
    }
    match status {
        Some(status) if status.success() => None,
        Some(status) => Some(ssh_exit_failure_message(status, &diagnostic)),
        None => Some(ssh_exit_timeout_message(&diagnostic)),
    }
}

fn ssh_exit_failure_message(status: ExitStatus, diagnostic: &str) -> String {
    if diagnostic.is_empty() {
        format!("ssh transport exited with {status}")
    } else {
        format!("ssh transport exited with {status}:\n{diagnostic}")
    }
}

fn ssh_exit_timeout_message(diagnostic: &str) -> String {
    if diagnostic.is_empty() {
        "ssh transport closed its output without exiting".to_owned()
    } else {
        format!("ssh transport closed its output without exiting:\n{diagnostic}")
    }
}

struct ConnectionSetup {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    child: Option<SshChild>,
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: String) -> Result<(), String> {
    tracing::info!(line_len = line.len(), "proxy router sending line to host");

    let envelope: protocol::Envelope = serde_json::from_str(&line)
        .map_err(|error| format!("failed to parse host envelope: {error}"))?;
    protocol::write_envelope(writer, &envelope)
        .await
        .map_err(|error| format!("failed to write host frame: {error}"))
}

fn emit_error(app: &AppHandle, host_id: &str, message: String) -> tauri::Result<()> {
    app.emit(
        HOST_ERROR_EVENT,
        HostErrorEvent {
            host_id: host_id.to_owned(),
            message,
        },
    )
}

fn emit_warning(app: &AppHandle, host_id: &str, message: String) -> tauri::Result<()> {
    app.emit(
        HOST_WARNING_EVENT,
        HostWarningEvent {
            host_id: host_id.to_owned(),
            message,
        },
    )
}

async fn capture_stderr<R, F>(
    mut stderr: R,
    capture: Arc<Mutex<StderrCapture>>,
    mut on_diagnostic: F,
) -> Result<(), String>
where
    R: AsyncRead + Unpin + Send,
    F: FnMut(String) + Send,
{
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stderr
            .read(&mut chunk)
            .await
            .map_err(|error| format!("failed to read ssh stderr: {error}"))?;
        if read == 0 {
            return Ok(());
        }
        let diagnostic = {
            let mut capture = capture.lock().expect("ssh stderr capture poisoned");
            capture.push(&chunk[..read]);
            capture.text()
        };
        if !diagnostic.is_empty() {
            on_diagnostic(diagnostic);
        }
    }
}

async fn setup_connection_transport(
    host_id: &str,
    app: AppHandle,
    transport: HostTransportConfig,
    host: server::HostHandle,
    live: Arc<AtomicBool>,
) -> Result<ConnectionSetup, String> {
    match transport {
        HostTransportConfig::LocalEmbedded => {
            let (client_stream, server_stream) = tokio::io::duplex(8192);
            let config = server::ServerConfig::current();

            tokio::spawn(async move {
                match server::accept(&config, server_stream).await {
                    Ok(conn) => {
                        if let Err(e) = server::run_connection(conn, host).await {
                            tracing::error!(?e, "server connection loop failed");
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e, "server handshake failed");
                    }
                }
            });

            let (read_half, write_half) = tokio::io::split(client_stream);
            Ok(ConnectionSetup {
                reader: Box::new(BufReader::new(read_half)),
                writer: Box::new(write_half),
                child: None,
            })
        }
        HostTransportConfig::SshStdio {
            ssh_destination,
            remote_command,
            lifecycle,
        } => {
            if ssh_destination.trim_start().starts_with('-') {
                return Err(format!(
                    "ssh destination for host {host_id} must not start with '-'"
                ));
            }
            let command = match lifecycle {
                RemoteHostLifecycleConfig::Manual => {
                    Ok(remote_command.unwrap_or_else(|| DEFAULT_REMOTE_HOST_COMMAND.to_string()))
                }
                RemoteHostLifecycleConfig::ManagedTyde => managed_remote_bridge_command(),
            }?;
            let mut child = Command::new("ssh");
            child
                .arg("-T")
                .arg(&ssh_destination)
                .arg(&command)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let mut child = child.spawn().map_err(|err| {
                format!("failed to start ssh transport for host {host_id}: {err}")
            })?;

            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| format!("ssh transport for host {host_id} has no stdout"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| format!("ssh transport for host {host_id} has no stdin"))?;

            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| format!("ssh transport for host {host_id} has no stderr"))?;
            let stderr_capture = Arc::new(Mutex::new(StderrCapture::default()));
            let stderr_capture_for_task = stderr_capture.clone();
            let app_for_stderr = app.clone();
            let host_id_for_stderr = host_id.to_string();
            let live_for_stderr = live.clone();
            let stderr_task = tokio::spawn(async move {
                capture_stderr(stderr, stderr_capture_for_task, move |diagnostic| {
                    tracing::warn!(
                        host_id = %host_id_for_stderr,
                        %diagnostic,
                        "ssh transport diagnostic"
                    );
                    if live_for_stderr.load(Ordering::Relaxed) {
                        let _ = emit_warning(
                            &app_for_stderr,
                            &host_id_for_stderr,
                            format!("ssh: {diagnostic}"),
                        );
                    }
                })
                .await
            });

            Ok(ConnectionSetup {
                reader: Box::new(BufReader::new(stdout)),
                writer: Box::new(stdin),
                child: Some(SshChild {
                    process: child,
                    stderr_capture,
                    stderr_task,
                }),
            })
        }
    }
}

fn managed_remote_bridge_command() -> Result<String, String> {
    let target_version = current_app_release_version()?;
    Ok(managed_remote_bridge_command_for_target(&target_version))
}

fn managed_remote_bridge_command_for_target(target_version: &TydeReleaseVersion) -> String {
    let target_version_sh = shell_quote(target_version.as_str());
    format!(
        r#"set -eu
mkdir -p "$HOME/.tyde/logs"
target_version={target_version_sh}
bin="$HOME/.tyde/bin/{target_version}/tyde-server"
if [ ! -x "$bin" ]; then
  echo "managed Tyde bridge requires exact target Tyde $target_version, but its binary is not executable: $bin" >&2
  exit 1
fi
export TYDE_SOCKET_PATH="$HOME/.tyde/tyde.sock"
exec "$bin" host --bridge-uds 2>> "$HOME/.tyde/logs/tyde-host-bridge-uds.log"
"#
    )
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    async fn diagnostic_process(script: &str) -> (SshChild, Arc<Mutex<Vec<String>>>) {
        let mut process = Command::new("/bin/sh");
        process
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut process = process.spawn().expect("spawn diagnostic process");
        let stderr = process.stderr.take().expect("capture process stderr");
        let stderr_capture = Arc::new(Mutex::new(StderrCapture::default()));
        let capture_for_task = stderr_capture.clone();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let updates_for_task = updates.clone();
        let stderr_task = tokio::spawn(async move {
            capture_stderr(stderr, capture_for_task, move |diagnostic| {
                updates_for_task
                    .lock()
                    .expect("diagnostic updates poisoned")
                    .push(diagnostic);
            })
            .await
        });
        (
            SshChild {
                process,
                stderr_capture,
                stderr_task,
            },
            updates,
        )
    }

    #[tokio::test]
    async fn ssh_diagnostics_follow_the_real_process_exit_status() {
        let (successful, successful_updates) = diagnostic_process(
            "printf '%s\\n' '** WARNING: connection is not using a post-quantum key exchange algorithm.' '**' >&2; exit 0",
        )
        .await;
        let success_error = finish_child(Some(successful), CloseCause::NaturalEof).await;
        assert_eq!(
            success_error, None,
            "stderr from a successful SSH process is diagnostic, not fatal"
        );
        let successful_diagnostic = successful_updates
            .lock()
            .expect("successful diagnostic updates poisoned")
            .last()
            .cloned()
            .expect("successful process must surface its warning");
        assert!(
            successful_diagnostic.contains("WARNING: connection is not using a post-quantum")
                && successful_diagnostic.ends_with("**"),
            "the cumulative warning must preserve every stderr line: {successful_diagnostic}"
        );

        let (failed, _) = diagnostic_process(
            "printf '%s\\n' 'Permission denied (publickey).' 'Connection closed by remote host.' >&2; exit 23",
        )
        .await;
        let failure = finish_child(Some(failed), CloseCause::NaturalEof)
            .await
            .expect("a nonzero SSH exit must be fatal");
        assert!(
            failure.contains("exit status: 23")
                && failure.contains("Permission denied (publickey).")
                && failure.contains("Connection closed by remote host."),
            "one failure must retain the exit status and complete stderr: {failure}"
        );
    }
}
