use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use protocol::{
    FrameKind, NewTerminalPayload, ProjectId, ProjectRootPath, TerminalErrorCode,
    TerminalErrorPayload, TerminalExitPayload, TerminalId, TerminalOutputPayload,
    TerminalSendPayload, TerminalStartPayload,
};
use tokio::sync::{Notify, mpsc};

use crate::stream::{Stream, StreamClosed};

#[derive(Debug, Clone)]
pub(crate) struct TerminalLaunchInfo {
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone)]
pub(crate) struct TerminalHandle {
    id: TerminalId,
    stream: Stream,
    start: TerminalStartPayload,
    state: Arc<TerminalState>,
    event_tx: mpsc::UnboundedSender<TerminalEvent>,
}

struct TerminalState {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send>>,
    exit: Mutex<Option<TerminalExitPayload>>,
    exit_notify: Notify,
}

enum TerminalEvent {
    Output(TerminalOutputPayload),
    Exit(TerminalExitPayload),
    Error(TerminalErrorPayload),
}

struct CreatedTerminalSession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send>,
    reader: Box<dyn Read + Send>,
    shell: String,
}

impl TerminalHandle {
    pub(crate) fn new_terminal_payload(&self) -> NewTerminalPayload {
        NewTerminalPayload {
            terminal_id: self.id.clone(),
            stream: self.stream.path().clone(),
        }
    }

    pub(crate) async fn emit_start(&self) -> Result<(), StreamClosed> {
        let payload =
            serde_json::to_value(&self.start).expect("failed to serialize terminal start payload");
        self.stream
            .send_value(FrameKind::TerminalStart, payload)
            .await
    }

    pub(crate) async fn send(&self, payload: TerminalSendPayload) {
        if let Some(message) = self.not_running_message("terminal not running") {
            self.emit_error(TerminalErrorPayload {
                code: TerminalErrorCode::NotRunning,
                message,
                fatal: false,
            });
            return;
        }
        let state = Arc::clone(&self.state);
        let data = payload.data;
        let result = tokio::task::spawn_blocking(move || {
            let mut writer = state.writer.lock().expect("terminal writer lock poisoned");
            writer
                .write_all(data.as_bytes())
                .map_err(|err| format!("failed to write terminal input: {err}"))?;
            writer
                .flush()
                .map_err(|err| format!("failed to flush terminal input: {err}"))?;
            Ok::<(), String>(())
        })
        .await
        .expect("terminal send task panicked");

        if let Err(message) = result {
            self.emit_runtime_failure(message);
        }
    }

    pub(crate) async fn resize(&self, cols: u16, rows: u16) {
        if let Some(message) = self.not_running_message("terminal not running") {
            self.emit_error(TerminalErrorPayload {
                code: TerminalErrorCode::NotRunning,
                message,
                fatal: false,
            });
            return;
        }

        let state = Arc::clone(&self.state);
        let result = tokio::task::spawn_blocking(move || {
            let master = state.master.lock().expect("terminal master lock poisoned");
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|err| format!("failed to resize terminal: {err}"))
        })
        .await
        .expect("terminal resize task panicked");

        if let Err(message) = result {
            self.emit_runtime_failure(message);
        }
    }

    pub(crate) async fn close(&self) {
        if self.state.exit_payload().is_some() {
            return;
        }

        let state = Arc::clone(&self.state);
        let result = tokio::task::spawn_blocking(move || {
            let mut child = state.child.lock().expect("terminal child lock poisoned");
            child
                .kill()
                .map_err(|err| format!("failed to kill terminal process: {err}"))
        })
        .await
        .expect("terminal close task panicked");

        if let Err(message) = result {
            self.emit_runtime_failure(message);
        }
    }

    pub(crate) async fn wait_for_exit(&self) -> TerminalExitPayload {
        loop {
            let notified = self.state.exit_notify.notified();
            if let Some(payload) = self.state.exit_payload() {
                return payload;
            }
            notified.await;
        }
    }

    fn not_running_message(&self, message: &str) -> Option<String> {
        if self.state.exit_payload().is_some() {
            return Some(message.to_owned());
        }
        None
    }

    fn emit_runtime_failure(&self, message: String) {
        if self.state.exit_payload().is_some() {
            self.emit_error(TerminalErrorPayload {
                code: TerminalErrorCode::NotRunning,
                message: "terminal not running".to_owned(),
                fatal: false,
            });
            return;
        }

        self.emit_error(TerminalErrorPayload {
            code: TerminalErrorCode::IoFailed,
            message,
            fatal: true,
        });
    }

    fn emit_error(&self, payload: TerminalErrorPayload) {
        let _ = self.event_tx.send(TerminalEvent::Error(payload));
    }
}

impl TerminalState {
    fn exit_payload(&self) -> Option<TerminalExitPayload> {
        self.exit
            .lock()
            .expect("terminal exit lock poisoned")
            .clone()
    }

    fn record_exit(&self, payload: TerminalExitPayload) -> bool {
        let mut exit = self.exit.lock().expect("terminal exit lock poisoned");
        if exit.is_some() {
            return false;
        }
        *exit = Some(payload);
        self.exit_notify.notify_waiters();
        true
    }
}

pub(crate) async fn create_terminal(
    launch: TerminalLaunchInfo,
    stream: Stream,
) -> Result<TerminalHandle, String> {
    let terminal_id = terminal_id_from_stream(stream.path());
    let create_launch = launch.clone();
    let created = tokio::task::spawn_blocking(move || create_terminal_session(&create_launch))
        .await
        .map_err(|err| format!("terminal create task panicked: {err}"))??;

    let state = Arc::new(TerminalState {
        master: Mutex::new(created.master),
        writer: Mutex::new(created.writer),
        child: Mutex::new(created.child),
        exit: Mutex::new(None),
        exit_notify: Notify::new(),
    });
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let sender_stream = stream.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let result = match event {
                TerminalEvent::Output(payload) => {
                    send_terminal_payload(&sender_stream, FrameKind::TerminalOutput, &payload).await
                }
                TerminalEvent::Exit(payload) => {
                    send_terminal_payload(&sender_stream, FrameKind::TerminalExit, &payload).await
                }
                TerminalEvent::Error(payload) => {
                    send_terminal_payload(&sender_stream, FrameKind::TerminalError, &payload).await
                }
            };

            if result.is_err() {
                return;
            }
        }
    });

    let handle = TerminalHandle {
        id: terminal_id,
        start: TerminalStartPayload {
            project_id: launch.project_id,
            root: launch.root,
            cwd: launch.cwd,
            shell: created.shell,
            cols: launch.cols,
            rows: launch.rows,
            created_at_ms: unix_time_ms(),
        },
        stream,
        state: Arc::clone(&state),
        event_tx: event_tx.clone(),
    };

    std::thread::spawn(move || {
        read_terminal_output(created.reader, state, event_tx);
    });

    Ok(handle)
}

fn create_terminal_session(launch: &TerminalLaunchInfo) -> Result<CreatedTerminalSession, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: launch.rows,
            cols: launch.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("failed to allocate PTY: {err}"))?;

    let (command, shell) = build_shell_command(&launch.cwd);
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|err| format!("failed to spawn terminal process: {err}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("failed to clone terminal reader: {err}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("failed to acquire terminal writer: {err}"))?;

    Ok(CreatedTerminalSession {
        master: pair.master,
        writer,
        child,
        reader,
        shell,
    })
}

fn read_terminal_output(
    mut reader: Box<dyn Read + Send>,
    state: Arc<TerminalState>,
    event_tx: mpsc::UnboundedSender<TerminalEvent>,
) {
    let mut buf = [0u8; 8192];
    let mut leftover = Vec::<u8>::new();

    loop {
        let read_start = leftover.len();
        if read_start >= buf.len() {
            emit_terminal_output(&event_tx, &leftover);
            leftover.clear();
            continue;
        }
        buf[..read_start].copy_from_slice(&leftover);
        leftover.clear();

        match reader.read(&mut buf[read_start..]) {
            Ok(0) => {
                if read_start > 0 {
                    emit_terminal_output(&event_tx, &buf[..read_start]);
                }
                emit_terminal_exit(&state, &event_tx);
                return;
            }
            Ok(n) => {
                let total = read_start + n;
                let data = &buf[..total];
                let emit_end = safe_emit_end(data);
                if emit_end == 0 {
                    leftover.extend_from_slice(data);
                    continue;
                }
                if emit_end < total {
                    leftover.extend_from_slice(&data[emit_end..total]);
                }
                emit_terminal_output(&event_tx, &data[..emit_end]);
            }
            Err(err) => {
                if read_start > 0 {
                    emit_terminal_output(&event_tx, &buf[..read_start]);
                }
                let _ = event_tx.send(TerminalEvent::Error(TerminalErrorPayload {
                    code: TerminalErrorCode::IoFailed,
                    message: format!("terminal stream read error: {err}"),
                    fatal: true,
                }));
                emit_terminal_exit(&state, &event_tx);
                return;
            }
        }
    }
}

fn emit_terminal_output(event_tx: &mpsc::UnboundedSender<TerminalEvent>, bytes: &[u8]) {
    let data = String::from_utf8_lossy(bytes).to_string();
    if data.is_empty() {
        return;
    }
    let _ = event_tx.send(TerminalEvent::Output(TerminalOutputPayload { data }));
}

fn emit_terminal_exit(state: &TerminalState, event_tx: &mpsc::UnboundedSender<TerminalEvent>) {
    let payload = if let Some(payload) = state.exit_payload() {
        payload
    } else {
        let status = {
            let mut child = state.child.lock().expect("terminal child lock poisoned");
            child.wait().ok()
        };
        terminal_exit_payload(status)
    };

    if state.record_exit(payload.clone()) {
        let _ = event_tx.send(TerminalEvent::Exit(payload));
    }
}

fn terminal_exit_payload(status: Option<ExitStatus>) -> TerminalExitPayload {
    let Some(status) = status else {
        return TerminalExitPayload {
            exit_code: None,
            signal: None,
        };
    };

    TerminalExitPayload {
        exit_code: Some(status.exit_code() as i32),
        signal: exit_signal(&status),
    }
}

fn exit_signal(status: &ExitStatus) -> Option<String> {
    let display = status.to_string();
    display
        .strip_prefix("Terminated by ")
        .map(|signal| signal.to_owned())
}

async fn send_terminal_payload<T: serde::Serialize>(
    stream: &Stream,
    kind: FrameKind,
    payload: &T,
) -> Result<(), StreamClosed> {
    let payload =
        serde_json::to_value(payload).expect("failed to serialize terminal stream payload");
    stream.send_value(kind, payload).await
}

fn terminal_id_from_stream(stream: &protocol::StreamPath) -> TerminalId {
    let terminal_id = stream
        .0
        .strip_prefix("/terminal/")
        .unwrap_or_else(|| panic!("invalid terminal stream path: {}", stream));
    TerminalId(terminal_id.to_owned())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_millis() as u64
}

fn build_shell_command(cwd: &str) -> (CommandBuilder, String) {
    let shell = default_shell();
    let mut command = CommandBuilder::new(shell.clone());
    if !cfg!(target_os = "windows") {
        command.arg("-l");
    }
    command.env("TERM", "xterm-256color");
    command.cwd(cwd);
    (command, shell)
}

fn default_shell() -> String {
    if cfg!(target_os = "windows") {
        return "powershell.exe".to_owned();
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_owned())
}

fn safe_emit_end(data: &[u8]) -> usize {
    let utf8_end = last_complete_utf8(data);
    if utf8_end == 0 {
        return 0;
    }

    let search_start = utf8_end.saturating_sub(64);
    let window = &data[search_start..utf8_end];
    if let Some(esc_offset) = window.iter().rposition(|&byte| byte == 0x1b) {
        let esc_pos = search_start + esc_offset;
        let seq = &data[esc_pos..utf8_end];
        if !ansi_sequence_complete(seq) {
            return esc_pos;
        }
    }

    utf8_end
}

fn last_complete_utf8(data: &[u8]) -> usize {
    let mut end = data.len();
    let mut continuations = 0;
    while end > 0 && continuations < 4 && (data[end - 1] & 0xC0) == 0x80 {
        end -= 1;
        continuations += 1;
    }
    if end == 0 {
        return 0;
    }

    let lead = data[end - 1];
    let expected_continuations = if lead < 0x80 {
        0
    } else if lead & 0xE0 == 0xC0 {
        1
    } else if lead & 0xF0 == 0xE0 {
        2
    } else if lead & 0xF8 == 0xF0 {
        3
    } else {
        return data.len();
    };

    if continuations == expected_continuations {
        data.len()
    } else {
        end - 1
    }
}

fn ansi_sequence_complete(seq: &[u8]) -> bool {
    debug_assert_eq!(seq[0], 0x1b);
    if seq.len() < 2 {
        return false;
    }

    match seq[1] {
        b'[' => {
            let last = seq[seq.len() - 1];
            (0x40..=0x7E).contains(&last) && seq.len() > 2
        }
        b']' | b'P' | b'X' | b'^' | b'_' => {
            seq.contains(&0x07) || seq.windows(2).any(|window| window == [0x1b, b'\\'])
        }
        0x40..=0x7E => true,
        _ => true,
    }
}
