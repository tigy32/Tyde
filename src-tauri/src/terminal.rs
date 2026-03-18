use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use serde_json::json;
use tauri::Emitter;

use crate::remote::{parse_remote_path, shell_quote_arg};

#[derive(Serialize, Clone)]
struct TerminalOutputPayload {
    terminal_id: u64,
    data: String,
}

#[derive(Serialize, Clone)]
struct TerminalExitPayload {
    terminal_id: u64,
    exit_code: Option<i32>,
}

struct TerminalSession {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send>,
}

pub struct TerminalManager {
    next_id: u64,
    sessions: HashMap<u64, TerminalSession>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            sessions: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn list_ids(&self) -> Vec<u64> {
        self.sessions.keys().copied().collect()
    }

    pub fn create_session(
        &mut self,
        app: tauri::AppHandle,
        workspace_path: &str,
    ) -> Result<u64, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to allocate PTY: {e}"))?;

        let cmd = if let Some(remote) = parse_remote_path(workspace_path) {
            build_remote_command(&remote.host, &remote.path)
        } else {
            build_local_command(workspace_path)
        };

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to spawn terminal process: {e}"))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("Failed to clone terminal reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("Failed to acquire terminal writer: {e}"))?;

        let id = self.next_id;
        self.next_id += 1;

        let app_output = app.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Leftover bytes from previous read: incomplete UTF-8 or trailing
            // incomplete ANSI escape sequence that would be corrupted if emitted.
            let mut leftover = Vec::<u8>::new();
            loop {
                let read_start = leftover.len();
                // Ensure leftover fits before the fresh read region.
                if read_start >= buf.len() {
                    // Extremely unlikely, but flush the leftover rather than deadlocking.
                    emit_terminal_output(&app_output, id, &leftover);
                    leftover.clear();
                    continue;
                }
                buf[..read_start].copy_from_slice(&leftover);
                leftover.clear();

                match reader.read(&mut buf[read_start..]) {
                    Ok(0) => {
                        // Flush any remaining leftover before exit.
                        if read_start > 0 {
                            emit_terminal_output(&app_output, id, &buf[..read_start]);
                        }
                        let payload = TerminalExitPayload {
                            terminal_id: id,
                            exit_code: None,
                        };
                        crate::record_debug_event_from_app(
                            &app_output,
                            "terminal_exit",
                            json!({
                                "terminal_id": id,
                                "exit_code": serde_json::Value::Null,
                            }),
                        );
                        let _ = app_output.emit("terminal-exit", payload);
                        break;
                    }
                    Ok(n) => {
                        let total = read_start + n;
                        let data = &buf[..total];

                        // Find the split point: keep back any trailing incomplete
                        // UTF-8 sequence and any trailing incomplete ANSI escape.
                        let emit_end = safe_emit_end(data);
                        if emit_end == 0 {
                            // Everything is incomplete — carry it all forward.
                            leftover.extend_from_slice(data);
                            continue;
                        }
                        if emit_end < total {
                            leftover.extend_from_slice(&data[emit_end..total]);
                        }
                        emit_terminal_output(&app_output, id, &data[..emit_end]);
                    }
                    Err(err) => {
                        tracing::warn!("Terminal reader error for session {id}: {err}");
                        // Flush any remaining leftover before exit.
                        if read_start > 0 {
                            emit_terminal_output(&app_output, id, &buf[..read_start]);
                        }
                        let payload = TerminalExitPayload {
                            terminal_id: id,
                            exit_code: None,
                        };
                        crate::record_debug_event_from_app(
                            &app_output,
                            "terminal_exit",
                            json!({
                                "terminal_id": id,
                                "exit_code": serde_json::Value::Null,
                                "error": err.to_string(),
                            }),
                        );
                        let _ = app_output.emit("terminal-exit", payload);
                        break;
                    }
                }
            }
        });

        self.sessions.insert(
            id,
            TerminalSession {
                master: pair.master,
                writer: Arc::new(Mutex::new(writer)),
                child,
            },
        );

        Ok(id)
    }

    pub fn write(&self, terminal_id: u64, data: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get(&terminal_id)
            .ok_or("Terminal session not found")?;
        let mut writer = session
            .writer
            .lock()
            .map_err(|_| "Terminal writer lock poisoned".to_string())?;
        writer
            .write_all(data.as_bytes())
            .map_err(|e| format!("Failed to write to terminal: {e}"))?;
        writer
            .flush()
            .map_err(|e| format!("Failed to flush terminal input: {e}"))?;
        Ok(())
    }

    pub fn resize(&self, terminal_id: u64, cols: u16, rows: u16) -> Result<(), String> {
        if cols < 2 || rows < 1 {
            return Err(format!(
                "Invalid terminal dimensions: cols={cols} (min 2), rows={rows} (min 1)"
            ));
        }

        let session = self
            .sessions
            .get(&terminal_id)
            .ok_or("Terminal session not found")?;
        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to resize terminal: {e}"))?;
        Ok(())
    }

    pub fn close(&mut self, terminal_id: u64) -> Result<(), String> {
        let mut session = self
            .sessions
            .remove(&terminal_id)
            .ok_or("Terminal session not found")?;
        let _ = session.child.kill();
        let _ = session.child.wait();
        Ok(())
    }

    pub fn close_all(&mut self) {
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        for id in ids {
            let _ = self.close(id);
        }
    }
}

/// Emit terminal output as a UTF-8-lossy string over the Tauri event bus.
fn emit_terminal_output(app: &tauri::AppHandle, terminal_id: u64, bytes: &[u8]) {
    let data = String::from_utf8_lossy(bytes).to_string();
    if data.is_empty() {
        return;
    }
    crate::record_debug_event_from_app(
        app,
        "terminal_output",
        json!({
            "terminal_id": terminal_id,
            "data": data.clone(),
        }),
    );
    let payload = TerminalOutputPayload {
        terminal_id,
        data,
    };
    let _ = app.emit("terminal-output", payload);
}

/// Return the number of leading bytes in `data` that are safe to emit without
/// splitting a multi-byte UTF-8 character or an in-progress ANSI escape sequence.
///
/// We first trim any trailing incomplete UTF-8 bytes, then check whether the
/// valid prefix ends with an incomplete ANSI escape (ESC not yet terminated).
fn safe_emit_end(data: &[u8]) -> usize {
    let utf8_end = last_complete_utf8(data);
    if utf8_end == 0 {
        return 0;
    }
    // Now check for a trailing incomplete ANSI escape sequence.
    // Scan backwards from the UTF-8-clean boundary looking for ESC (0x1b).
    // ANSI CSI sequences: ESC [ <params> <intermediate> <final byte>
    //   Final byte is 0x40..=0x7E.
    // ANSI OSC sequences: ESC ] ... ST  (ST = ESC \ or BEL 0x07)
    // Simple two-byte escapes: ESC <final>, final in 0x40..=0x7E
    //
    // We only need to protect a *trailing* incomplete escape. Walk backwards
    // from the end (max ~64 bytes — escape sequences are short) looking for ESC.
    let search_start = utf8_end.saturating_sub(64);
    let window = &data[search_start..utf8_end];
    if let Some(esc_offset) = window.iter().rposition(|&b| b == 0x1b) {
        let esc_pos = search_start + esc_offset;
        let seq = &data[esc_pos..utf8_end];
        if !ansi_sequence_complete(seq) {
            // The escape sequence starting at esc_pos is incomplete.
            return esc_pos;
        }
    }
    utf8_end
}

/// Return the length of the longest prefix of `data` that is valid UTF-8 at
/// character boundaries (i.e. does not end in the middle of a multi-byte char).
fn last_complete_utf8(data: &[u8]) -> usize {
    // A UTF-8 continuation byte starts with 10xxxxxx (0x80..=0xBF).
    // Walk backwards past any trailing continuation bytes, then check if the
    // leading byte expects more continuations than are present.
    let mut end = data.len();
    // Count trailing continuation bytes (max 3 for valid UTF-8).
    let mut conts = 0;
    while end > 0 && conts < 4 && (data[end - 1] & 0xC0) == 0x80 {
        end -= 1;
        conts += 1;
    }
    if end == 0 {
        // All bytes are continuation bytes — nothing to emit.
        return 0;
    }
    let lead = data[end - 1];
    let expected_conts = if lead < 0x80 {
        0
    } else if lead & 0xE0 == 0xC0 {
        1
    } else if lead & 0xF0 == 0xE0 {
        2
    } else if lead & 0xF8 == 0xF0 {
        3
    } else {
        // Invalid leading byte — treat as complete (lossy decode will handle it).
        return data.len();
    };
    if conts == expected_conts {
        // The trailing multi-byte char is complete.
        data.len()
    } else {
        // Incomplete multi-byte char — exclude it.
        end - 1
    }
}

/// Returns `true` if the byte slice starting with ESC represents a complete
/// ANSI escape sequence (or at least not an obviously incomplete one).
fn ansi_sequence_complete(seq: &[u8]) -> bool {
    debug_assert!(seq[0] == 0x1b);
    if seq.len() < 2 {
        // Bare ESC — incomplete.
        return false;
    }
    match seq[1] {
        b'[' => {
            // CSI sequence: ESC [ <params> <final>
            // Final byte is in 0x40..=0x7E.
            // Check if last byte is a valid final byte.
            let last = seq[seq.len() - 1];
            (0x40..=0x7E).contains(&last) && seq.len() > 2
        }
        b']' => {
            // OSC sequence: ESC ] ... (terminated by BEL or ESC \)
            // Check for BEL (0x07) or ST (ESC \) anywhere in the sequence.
            seq.contains(&0x07)
                || seq.windows(2).any(|w| w == [0x1b, b'\\'])
        }
        b'P' | b'X' | b'^' | b'_' => {
            // DCS, SOS, PM, APC — string sequences terminated by ST.
            seq.contains(&0x07)
                || seq.windows(2).any(|w| w == [0x1b, b'\\'])
        }
        0x40..=0x7E => {
            // Two-byte escape (ESC + final). Complete at length 2.
            true
        }
        _ => {
            // Unknown — treat as complete to avoid holding data forever.
            true
        }
    }
}

fn default_shell() -> String {
    if cfg!(target_os = "windows") {
        return "powershell.exe".to_string();
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

fn build_local_command(workspace_path: &str) -> CommandBuilder {
    let shell = default_shell();
    let mut cmd = CommandBuilder::new(shell);
    if !cfg!(target_os = "windows") {
        cmd.arg("-l");
    }
    cmd.env("TERM", "xterm-256color");
    cmd.cwd(workspace_path);
    cmd
}

fn build_remote_command(host: &str, path: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("ssh");
    cmd.arg("-tt");
    cmd.arg(host);
    let remote_cmd = format!(
        "cd {} && exec ${{SHELL:-/bin/bash}} -l",
        shell_quote_arg(path)
    );
    cmd.arg(remote_cmd);
    cmd.env("TERM", "xterm-256color");
    cmd
}
