use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use protocol::{
    FrameKind, NewTerminalPayload, ProjectId, ProjectRootPath, TerminalBootstrapPayload,
    TerminalErrorCode, TerminalErrorPayload, TerminalExitPayload, TerminalId,
    TerminalOutputPayload, TerminalSendPayload, TerminalStartPayload,
};
use tokio::sync::{Notify, mpsc};

use crate::process_env;
use crate::stream::{Stream, StreamClosed};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalLaunchCommand {
    DefaultShell,
    Trusted {
        program: String,
        arguments: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct TerminalLaunchInfo {
    pub project_id: Option<ProjectId>,
    pub root: Option<ProjectRootPath>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub command: TerminalLaunchCommand,
}

#[derive(Clone)]
pub(crate) struct TerminalHandle {
    id: TerminalId,
    stream: Stream,
    start: TerminalStartPayload,
    state: Arc<TerminalState>,
    event_tx: mpsc::UnboundedSender<TerminalEvent>,
    startup: Arc<Mutex<Option<TerminalIoStartup>>>,
    io_started: Arc<AtomicBool>,
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

struct TerminalIoStartup {
    reader: Box<dyn Read + Send>,
    event_rx: mpsc::UnboundedReceiver<TerminalEvent>,
}

impl TerminalHandle {
    pub(crate) fn new_terminal_payload(&self) -> NewTerminalPayload {
        NewTerminalPayload {
            terminal_id: self.id.clone(),
            stream: self.stream.path().clone(),
        }
    }

    pub(crate) fn project_id(&self) -> Option<&ProjectId> {
        self.start.project_id.as_ref()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state.exit_payload().is_none()
    }

    pub(crate) async fn emit_bootstrap_and_start_io(&self) -> Result<(), StreamClosed> {
        let payload = serde_json::to_value(TerminalBootstrapPayload {
            terminal_id: self.id.clone(),
            start: self.start.clone(),
        })
        .expect("failed to serialize terminal bootstrap payload");
        self.stream
            .send_value(FrameKind::TerminalBootstrap, payload)?;
        self.start_io();
        Ok(())
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

    fn start_io(&self) {
        if self.io_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let startup = self
            .startup
            .lock()
            .expect("terminal startup lock poisoned")
            .take();
        let Some(TerminalIoStartup {
            reader,
            mut event_rx,
        }) = startup
        else {
            return;
        };

        let sender_stream = self.stream.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let result = match event {
                    TerminalEvent::Output(payload) => {
                        send_terminal_payload(&sender_stream, FrameKind::TerminalOutput, &payload)
                            .await
                    }
                    TerminalEvent::Exit(payload) => {
                        send_terminal_payload(&sender_stream, FrameKind::TerminalExit, &payload)
                            .await
                    }
                    TerminalEvent::Error(payload) => {
                        send_terminal_payload(&sender_stream, FrameKind::TerminalError, &payload)
                            .await
                    }
                };

                if result.is_err() {
                    return;
                }
            }
        });

        let state = Arc::clone(&self.state);
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            read_terminal_output(reader, state, event_tx);
        });
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
    let (event_tx, event_rx) = mpsc::unbounded_channel();

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
        startup: Arc::new(Mutex::new(Some(TerminalIoStartup {
            reader: created.reader,
            event_rx,
        }))),
        io_started: Arc::new(AtomicBool::new(false)),
    };

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

    let (command, shell) = build_terminal_command(launch);
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
        debug_assert!(read_start <= 3, "incomplete UTF-8 suffix exceeded 3 bytes");
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
                let emit_end = last_complete_utf8(data);
                debug_assert!(
                    total - emit_end <= 3,
                    "incomplete UTF-8 suffix exceeded 3 bytes"
                );
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
    stream.send_value(kind, payload)
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

fn build_terminal_command(launch: &TerminalLaunchInfo) -> (CommandBuilder, String) {
    let (mut command, program) = match &launch.command {
        TerminalLaunchCommand::DefaultShell => {
            let shell = default_shell();
            let mut command = CommandBuilder::new(shell.clone());
            if !cfg!(target_os = "windows") {
                command.arg("-l");
            }
            (command, shell)
        }
        TerminalLaunchCommand::Trusted { program, arguments } => {
            let mut command = CommandBuilder::new(program);
            command.args(arguments);
            (command, program.clone())
        }
    };
    command.env("TERM", "xterm-256color");
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    command.cwd(&launch.cwd);
    (command, program)
}

fn default_shell() -> String {
    if cfg!(target_os = "windows") {
        return "powershell.exe".to_owned();
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_owned())
}

fn last_complete_utf8(data: &[u8]) -> usize {
    let mut sequence_start = data.len();
    while sequence_start > 0
        && data.len() - sequence_start < 3
        && (data[sequence_start - 1] & 0xC0) == 0x80
    {
        sequence_start -= 1;
    }
    if sequence_start == 0 {
        return data.len();
    }

    let lead_index = sequence_start - 1;
    let sequence_width = match data[lead_index] {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return data.len(),
    };

    if data.len() - lead_index < sequence_width {
        lead_index
    } else {
        data.len()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{
        TerminalLaunchCommand, TerminalLaunchInfo, build_terminal_command, last_complete_utf8,
    };

    fn emitted_chunks(reads: &[&[u8]], flush_pending: bool) -> Vec<String> {
        let mut chunks = Vec::new();
        let mut pending = Vec::new();

        for read in reads {
            pending.extend_from_slice(read);
            let emit_end = last_complete_utf8(&pending);
            assert!(pending.len() - emit_end <= 3);
            if emit_end > 0 {
                chunks.push(String::from_utf8_lossy(&pending[..emit_end]).into_owned());
                pending.drain(..emit_end);
            }
        }

        if flush_pending && !pending.is_empty() {
            chunks.push(String::from_utf8_lossy(&pending).into_owned());
        }

        chunks
    }

    #[test]
    fn complete_csi_and_prompt_text_emit_immediately() {
        let output = b"\x1b[2Kready\n$ ";
        assert_eq!(emitted_chunks(&[output], false), vec!["\x1b[2Kready\n$ "]);
    }

    #[test]
    fn split_ansi_is_not_aligned_by_the_server() {
        let reads: &[&[u8]] = &[b"\x1b[", b"31mred", b"\x1b", b"[0m"];
        let chunks = emitted_chunks(reads, false);
        assert_eq!(chunks, vec!["\x1b[", "31mred", "\x1b", "[0m"]);
        assert_eq!(chunks.concat(), "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn split_multibyte_utf8_reassembles_exactly() {
        for expected in ["¢", "€", "🦀"] {
            let bytes = expected.as_bytes();
            let reads = bytes.iter().map(std::slice::from_ref).collect::<Vec<_>>();
            assert_eq!(emitted_chunks(&reads, false), vec![expected]);
        }
    }

    #[test]
    fn complete_ascii_and_utf8_emit_immediately() {
        assert_eq!(emitted_chunks(&[b"plain text"], false), vec!["plain text"]);
        assert_eq!(
            emitted_chunks(&["café 🦀".as_bytes()], false),
            vec!["café 🦀"]
        );
    }

    #[test]
    fn eof_flushes_pending_partial_utf8_lossily() {
        let partial_crab = &[0xF0, 0x9F, 0xA6];
        assert_eq!(emitted_chunks(&[b"ok", partial_crab], false), vec!["ok"]);
        assert_eq!(
            emitted_chunks(&[b"ok", partial_crab], true),
            vec!["ok", "�"]
        );
    }

    #[test]
    fn trusted_command_uses_exact_program_and_arguments() {
        let launch = TerminalLaunchInfo {
            project_id: None,
            root: None,
            cwd: "/tmp".to_owned(),
            cols: 100,
            rows: 28,
            command: TerminalLaunchCommand::Trusted {
                program: "/bin/sh".to_owned(),
                arguments: vec!["/tmp/tyde setup script.sh".to_owned()],
            },
        };

        let (command, program) = build_terminal_command(&launch);

        assert_eq!(program, "/bin/sh");
        assert_eq!(
            command.get_argv(),
            &[
                OsString::from("/bin/sh"),
                OsString::from("/tmp/tyde setup script.sh"),
            ]
        );
    }
}
