use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use server::process_env::launch::{AsyncCommandGroup, AsyncGroupChild as Child, ProcessStdio};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::bridge::{
    HOST_DISCONNECTED_EVENT, HOST_ERROR_EVENT, HOST_LINE_EVENT, HOST_WARNING_EVENT,
    HostDisconnectedEvent, HostErrorEvent, HostLineEvent, HostWarningEvent,
};
use crate::host_store::{HostTransportConfig, RemoteHostLifecycleConfig};
use crate::remote_bootstrap::{current_app_release_version, shell_quote};
use host_config::TydeReleaseVersion;

const REMOTE_HOST_COMMAND: &str = "tyde host --bridge-uds";
const SSH_STDERR_CAPTURE_LIMIT: usize = 64 * 1024;
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
/// and generation-specific cancellation/completion channels). It never sits in the inbound data
/// path, so a stalled/backpressured writer can never stop the reader from
/// draining the transport — which is what previously deadlocked the SSH proxy.
#[derive(Clone)]
pub struct ProxyRouterHandle {
    state: Arc<Mutex<RouterState>>,
}

struct RouterState {
    hosts: HashMap<String, Connection>,
    next_connection_id: u64,
    pending: HashMap<String, ConnectionControl>,
}

struct Connection {
    connection_id: u64,
    /// Outbound frames are enqueued here; the writer task owns the receiver.
    /// Dropping every sender (i.e. removing this entry) makes the writer task
    /// finish on its own.
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    control: ConnectionControl,
    /// Liveness flag shared with this connection's reader/stderr tasks. Cleared
    /// (non-blocking, no lock) the instant the entry is removed or superseded,
    /// so a stale reader/stderr task that is still draining stops emitting
    /// instead of leaking late frames/errors into a newer connection that
    /// reused the same host id.
    live: Arc<AtomicBool>,
    audio_pending: Arc<AtomicUsize>,
    available: Arc<AtomicBool>,
    retry: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
struct ConnectionControl {
    live: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<Option<Result<(), String>>>,
}

struct ConnectionOwner {
    cancel: watch::Receiver<bool>,
    finished: watch::Sender<Option<Result<(), String>>>,
}

impl ConnectionControl {
    fn new() -> (Self, ConnectionOwner) {
        let (cancel, cancellation) = watch::channel(false);
        let (finished, completion) = watch::channel(None);
        (
            Self {
                live: Arc::new(AtomicBool::new(true)),
                cancel,
                finished: completion,
            },
            ConnectionOwner {
                cancel: cancellation,
                finished,
            },
        )
    }

    fn cancel(&self) {
        self.live.store(false, Ordering::Release);
        self.cancel.send_replace(true);
    }

    async fn settled(mut self) -> Result<(), String> {
        loop {
            if let Some(result) = self.finished.borrow_and_update().clone() {
                return result;
            }
            self.finished
                .changed()
                .await
                .map_err(|_| "connection owner ended without completing SSH cleanup".to_owned())?;
        }
    }
}

impl ConnectionOwner {
    async fn cancelled(&mut self) {
        let _ = self.cancel.wait_for(|cancelled| *cancelled).await;
    }

    fn finish(self, result: Result<(), String>) {
        self.finished.send_replace(Some(result));
    }
}

struct PendingConnection {
    state: Arc<Mutex<RouterState>>,
    host_id: String,
    live: Arc<AtomicBool>,
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("router state poisoned");
        if state
            .pending
            .get(&self.host_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.live, &self.live))
        {
            state.pending.remove(&self.host_id);
            self.live.store(false, Ordering::Release);
        }
    }
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

impl ProxyRouterHandle {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RouterState {
                hosts: HashMap::new(),
                next_connection_id: 1,
                pending: HashMap::new(),
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
        let (control, owner) = ConnectionControl::new();
        let live = control.live.clone();
        let (previous, existing) = {
            let mut guard = self.state.lock().expect("router state poisoned");
            let previous = guard.pending.insert(host_id.clone(), control.clone());
            if let Some(old) = &previous {
                old.cancel();
            }
            let existing = guard.hosts.remove(&host_id);
            if let Some(old) = &existing {
                old.control.cancel();
            }
            (previous, existing)
        };
        let pending = PendingConnection {
            state: self.state.clone(),
            host_id: host_id.clone(),
            live: live.clone(),
        };
        let cleanup = async {
            if let Some(previous) = previous {
                previous.settled().await?;
            }
            if let Some(existing) = existing {
                tracing::info!(host_id, "replacing existing host connection");
                #[cfg(not(target_os = "windows"))]
                if let Err(error) = app
                    .state::<crate::ShellState>()
                    .voice_media
                    .stop_for_host(&host_id)
                {
                    let _ = emit_error(&app, &host_id, error);
                }
                existing.control.settled().await?;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = cleanup {
            owner.finish(Err(error.clone()));
            return Err(error);
        }
        if !live.load(Ordering::Acquire) {
            owner.finish(Ok(()));
            return Err("connection attempt cancelled".into());
        }

        let connection_id = {
            let mut guard = self.state.lock().expect("router state poisoned");
            let id = guard.next_connection_id;
            guard.next_connection_id += 1;
            id
        };

        let available = Arc::new(AtomicBool::new(true));
        let retry = Arc::new(tokio::sync::Notify::new());
        let resumable = matches!(&transport, HostTransportConfig::SshStdio { .. });
        let setup = setup_connection_transport(
            &host_id,
            app.clone(),
            transport.clone(),
            host.clone(),
            live.clone(),
        )
        .await;
        let setup = match setup {
            Ok(setup) => setup,
            Err(error) => {
                owner.finish(Err(error.clone()));
                return Err(error);
            }
        };
        let setup = if resumable {
            setup_recovery(
                RecoveryContext {
                    host_id: host_id.clone(),
                    app: app.clone(),
                    transport,
                    host,
                    live: live.clone(),
                    available: available.clone(),
                    retry: retry.clone(),
                },
                setup,
                owner,
            )
            .await?
        } else {
            owner.finish(Ok(()));
            setup
        };

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<Outbound>();
        let audio_pending = Arc::new(AtomicUsize::new(0));

        // Register before spawning so that an immediate reader EOF can find the
        // entry (and thus reap the child / emit disconnect) instead of racing.
        let registered = {
            let mut guard = self.state.lock().expect("router state poisoned");
            if guard
                .pending
                .get(&host_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.live, &live))
                && live.load(Ordering::Acquire)
            {
                guard.pending.remove(&host_id);
                guard.hosts.insert(
                    host_id.clone(),
                    Connection {
                        connection_id,
                        outbound_tx,
                        control: control.clone(),
                        live: live.clone(),
                        audio_pending,
                        available,
                        retry,
                    },
                );
                true
            } else {
                false
            }
        };
        if !registered {
            control.cancel();
            abort_child(setup.child).await?;
            control.settled().await?;
            return Err("connection attempt cancelled".into());
        }

        drop(pending);
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
        let (pending, connection) = {
            let mut guard = self.state.lock().expect("router state poisoned");
            // Keep the pending generation visible until its owner completes,
            // so simultaneous disconnects and retries join the same cleanup.
            let pending = guard.pending.get(&host_id).cloned();
            if let Some(pending) = &pending {
                pending.cancel();
            }
            let connection = guard.hosts.remove(&host_id);
            if let Some(connection) = &connection {
                connection.control.cancel();
                guard
                    .pending
                    .insert(host_id.clone(), connection.control.clone());
            }
            (pending, connection)
        };
        if let Some(pending) = pending {
            pending.settled().await?;
        }
        if let Some(connection) = connection {
            let retiring = PendingConnection {
                state: self.state.clone(),
                host_id,
                live: connection.live.clone(),
            };
            let result = connection.control.settled().await;
            drop(retiring);
            result?;
        }
        Ok(())
    }

    pub fn retry(&self, host_id: &str) -> Result<(), String> {
        let state = self.state.lock().expect("router state poisoned");
        let connection = state
            .hosts
            .get(host_id)
            .ok_or_else(|| "host needs a fresh connection".to_owned())?;
        connection.retry.notify_one();
        Ok(())
    }

    pub async fn send_line(&self, host_id: String, line: String) -> Result<(), String> {
        if line.contains('\n') {
            return Err("host line must not contain a newline".to_owned());
        }

        let stopping_voice = serde_json::from_str::<protocol::Envelope>(&line)
            .is_ok_and(|envelope| envelope.kind == protocol::FrameKind::VoiceStop);
        let outbound_tx = {
            let guard = self.state.lock().expect("router state poisoned");
            guard
                .hosts
                .get(&host_id)
                .filter(|c| c.available.load(Ordering::Acquire) || stopping_voice)
                .map(|c| c.outbound_tx.clone())
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
            .filter(|c| c.available.load(Ordering::Acquire))
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
    loop {
        match reader.read_frame().await {
            Ok(None) => {
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

    close_connection(&state, &app, &host_id, connection_id).await;
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
            close_connection(&state, &app, &host_id, connection_id).await;
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
) {
    let connection = {
        let mut guard = state.lock().expect("router state poisoned");
        match guard.hosts.get(host_id) {
            Some(existing) if existing.connection_id == connection_id => {
                let connection = guard.hosts.remove(host_id);
                if let Some(connection) = &connection {
                    connection.control.cancel();
                    guard
                        .pending
                        .insert(host_id.to_owned(), connection.control.clone());
                }
                connection
            }
            _ => None,
        }
    };
    let Some(connection) = connection else {
        return;
    };

    let retiring = PendingConnection {
        state: state.clone(),
        host_id: host_id.to_owned(),
        live: connection.live.clone(),
    };
    #[cfg(not(target_os = "windows"))]
    if let Err(error) = app
        .state::<crate::ShellState>()
        .voice_media
        .stop_for_host(host_id)
    {
        let _ = emit_error(app, host_id, error);
    }
    if let Err(error) = connection.control.settled().await {
        let _ = emit_error(app, host_id, error);
    }
    tracing::info!(host_id, connection_id, "connection closed");
    {
        let guard = state.lock().expect("router state poisoned");
        if guard
            .pending
            .get(host_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.live, &connection.live))
        {
            let _ = app.emit(
                HOST_DISCONNECTED_EVENT,
                HostDisconnectedEvent {
                    host_id: host_id.to_owned(),
                },
            );
        }
    }
    drop(retiring);
}

async fn abort_child(child: Option<SshChild>) -> Result<(), String> {
    let Some(mut child) = child else {
        return Ok(());
    };
    child
        .process
        .kill()
        .await
        .map_err(|error| format!("failed to cancel SSH transport: {error}"))?;
    let stderr = child
        .stderr_task
        .await
        .map_err(|error| format!("failed to join SSH stderr capture: {error}"))?;
    stderr?;
    tracing::info!("SSH transport cancellation completed");
    Ok(())
}

async fn finish_child(child: &mut Option<SshChild>) -> Result<Option<String>, String> {
    let Some(child) = child.as_mut() else {
        return Ok(None);
    };
    let status = child
        .process
        .wait()
        .await
        .map_err(|error| format!("failed to wait for ssh transport exit: {error}"))?;
    let stderr_result = (&mut child.stderr_task).await;
    let diagnostic = child
        .stderr_capture
        .lock()
        .expect("ssh stderr capture poisoned")
        .text();

    match stderr_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(format!("failed to join ssh stderr capture: {error}")),
    }
    Ok(if status.success() {
        None
    } else {
        Some(ssh_exit_failure_message(status, &diagnostic))
    })
}

fn ssh_exit_failure_message(status: ExitStatus, diagnostic: &str) -> String {
    if diagnostic.is_empty() {
        format!("ssh transport exited with {status}")
    } else {
        format!("ssh transport exited with {status}:\n{diagnostic}")
    }
}

async fn finish_transport(mut child: Option<SshChild>) -> Result<Option<String>, String> {
    if let Some(child) = &mut child {
        child
            .process
            .start_kill()
            .map_err(|error| format!("failed to end SSH transport: {error}"))?;
    }
    finish_child(&mut child).await
}

struct ConnectionSetup {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    child: Option<SshChild>,
}

struct RecoveryContext {
    host_id: String,
    app: AppHandle,
    transport: HostTransportConfig,
    host: server::HostHandle,
    live: Arc<AtomicBool>,
    available: Arc<AtomicBool>,
    retry: Arc<tokio::sync::Notify>,
}

impl RecoveryContext {
    fn status(&self, connected: bool, attempt: u32, retry_in_seconds: u32, message: String) {
        if !self.live.load(Ordering::Acquire) {
            return;
        }
        let _ = self.app.emit(
            "tyde://host-recovery",
            host_config::HostRecoveryEvent {
                host_id: self.host_id.clone(),
                connected,
                attempt,
                retry_in_seconds,
                message,
            },
        );
    }
}

async fn setup_recovery(
    context: RecoveryContext,
    mut setup: ConnectionSetup,
    mut owner: ConnectionOwner,
) -> Result<ConnectionSetup, String> {
    let (session, logical) = server::recovery::Session::new();
    let handshake = tokio::select! {
        biased;
        _ = owner.cancelled() => {
            session.close();
            let cleanup = abort_child(setup.child).await;
            owner.finish(cleanup.clone());
            cleanup?;
            return Err("connection attempt cancelled".into());
        }
        handshake = server::recovery::connect(&mut setup.reader, &mut setup.writer, None, &session) => handshake,
    };
    let (id, transport, tail) = match handshake {
        Ok(handshake) => handshake,
        Err(error) => {
            session.close();
            let diagnostic = finish_transport(setup.child).await;
            let cancelled = *owner.cancel.borrow();
            owner.finish(diagnostic.as_ref().map(|_| ()).map_err(Clone::clone));
            let diagnostic = diagnostic?;
            if cancelled {
                return Err("connection attempt cancelled".into());
            }
            return Err(format!(
                "recovery handshake failed: {error}; {}",
                diagnostic.unwrap_or_default()
            ));
        }
    };
    tokio::spawn(async move {
        let result = supervise_recovery(
            context,
            setup,
            session.clone(),
            id,
            transport,
            tail,
            &mut owner,
        )
        .await;
        session.close();
        owner.finish(result);
    });
    let (reader, writer) = tokio::io::split(logical);
    Ok(ConnectionSetup {
        reader: Box::new(BufReader::new(reader)),
        writer: Box::new(writer),
        child: None,
    })
}

async fn supervise_recovery(
    context: RecoveryContext,
    mut setup: ConnectionSetup,
    session: Arc<server::recovery::Session>,
    id: uuid::Uuid,
    mut transport: server::recovery::Attachment,
    mut tail: u64,
    owner: &mut ConnectionOwner,
) -> Result<(), String> {
    let mut recovering = false;
    loop {
        let attached_at = std::time::Instant::now();
        let attachment = async {
            let attachment = transport.run(setup.reader, setup.writer);
            tokio::pin!(attachment);
            if recovering {
                context.status(false, 0, 0, "Connected — catching up…".into());
                tokio::select! {
                    result = &mut attachment => result,
                    _ = session.caught_up(tail) => {
                        if !session.is_closed() && context.live.load(Ordering::Acquire) {
                            context.available.store(true, Ordering::Release);
                            context.status(true, 0, 0, "Connected".into());
                        }
                        attachment.await
                    }
                }
            } else {
                attachment.await
            }
        };
        let result = tokio::select! {
            biased;
            _ = owner.cancelled() => {
                session.close();
                return abort_child(setup.child).await;
            }
            _ = session.closed() => return abort_child(setup.child).await,
            result = attachment => result,
        };
        context.available.store(false, Ordering::Release);
        #[cfg(not(target_os = "windows"))]
        if let Err(error) = context
            .app
            .state::<crate::ShellState>()
            .voice_media
            .stop_for_host(&context.host_id)
        {
            tracing::warn!(%error, "failed to stop voice after transport loss");
        }
        context.status(false, 1, 0, "SSH transport disconnected".into());
        let diagnostic = finish_transport(setup.child).await?;
        let mut reason = format!(
            "{}{}",
            result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "SSH transport closed".into()),
            diagnostic.map(|d| format!("; {d}")).unwrap_or_default()
        );
        tracing::warn!(host_id = %context.host_id, connection_age_seconds = attached_at.elapsed().as_secs(),
                received = session.received(), sent = session.tail(), %reason, "SSH session detached");
        if session.is_closed() {
            return Ok(());
        }
        recovering = true;
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            context.status(false, attempt, 1, reason.clone());
            tokio::select! {
                biased;
                _ = owner.cancelled() => { session.close(); return Ok(()); },
                _ = session.closed() => return Ok(()),
                _ = context.retry.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
            context.status(false, attempt, 0, reason.clone());
            let next = setup_connection_transport(
                &context.host_id,
                context.app.clone(),
                context.transport.clone(),
                context.host.clone(),
                context.live.clone(),
            )
            .await;
            let mut next = match next {
                Ok(next) => next,
                Err(error) => {
                    reason = error;
                    continue;
                }
            };
            let handshake = tokio::select! {
                biased;
                _ = owner.cancelled() => {
                    session.close();
                    return abort_child(next.child).await;
                }
                _ = session.closed() => return abort_child(next.child).await,
                handshake = server::recovery::connect(
                    &mut next.reader,
                    &mut next.writer,
                    Some(id),
                    &session,
                ) => handshake,
            };
            match handshake {
                Ok((_, next_transport, next_tail)) => {
                    setup = next;
                    transport = next_transport;
                    tail = next_tail;
                    break;
                }
                Err(error) => {
                    let expired = error.kind() == std::io::ErrorKind::NotFound;
                    reason = error.to_string();
                    if let Some(diagnostic) = finish_transport(next.child).await? {
                        reason.push_str(&format!("; {diagnostic}"));
                    }
                    tracing::warn!(host_id = %context.host_id, attempt, %reason, "SSH reconnect attempt failed");
                    if expired {
                        context.status(
                            false,
                            attempt,
                            0,
                            "Session expired — refreshing workspace…".into(),
                        );
                        session.close();
                        return Ok(());
                    }
                }
            }
        }
    }
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
            lifecycle,
        } => {
            if ssh_destination.trim_start().starts_with('-') {
                return Err(format!(
                    "ssh destination for host {host_id} must not start with '-'"
                ));
            }
            let command = match lifecycle {
                RemoteHostLifecycleConfig::Manual => Ok(REMOTE_HOST_COMMAND.to_string()),
                RemoteHostLifecycleConfig::ManagedTyde => managed_remote_bridge_command(),
            }?;
            let mut child = crate::devtools::ssh_command()?;
            child
                .arg("-T")
                .args([
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "BatchMode=yes",
                ])
                .arg(&ssh_destination)
                .arg(&command)
                .stdin(ProcessStdio::piped())
                .stdout(ProcessStdio::piped())
                .stderr(ProcessStdio::piped());

            let mut child = child.group_spawn().await.map_err(|err| {
                format!("failed to start ssh transport for host {host_id}: {err}")
            })?;

            let (Some(stdout), Some(stdin), Some(stderr)) =
                (child.take_stdout(), child.take_stdin(), child.take_stderr())
            else {
                child.kill().await.map_err(|error| {
                    format!("failed to cancel SSH after missing pipes: {error}")
                })?;
                return Err(format!(
                    "ssh transport for host {host_id} is missing owned pipes"
                ));
            };

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
        let mut process =
            server::process_env::command("/bin/sh").expect("resolve login-shell PATH");
        process
            .arg("-c")
            .arg(script)
            .stdin(ProcessStdio::null())
            .stdout(ProcessStdio::null())
            .stderr(ProcessStdio::piped());
        let mut process = process
            .group_spawn()
            .await
            .expect("spawn diagnostic process");
        let stderr = process.take_stderr().expect("capture process stderr");
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
        let success_error = finish_child(&mut Some(successful))
            .await
            .expect("join successful process");
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
        let failure = finish_child(&mut Some(failed))
            .await
            .expect("join failed process")
            .expect("a nonzero SSH exit must be fatal");
        assert!(
            failure.contains("exit status: 23")
                && failure.contains("Permission denied (publickey).")
                && failure.contains("Connection closed by remote host."),
            "one failure must retain the exit status and complete stderr: {failure}"
        );
    }
}
