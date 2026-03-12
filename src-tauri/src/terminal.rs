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
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
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
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        if data.is_empty() {
                            continue;
                        }
                        crate::record_debug_event_from_app(
                            &app_output,
                            "terminal_output",
                            json!({
                                "terminal_id": id,
                                "data": data.clone(),
                            }),
                        );
                        let payload = TerminalOutputPayload {
                            terminal_id: id,
                            data,
                        };
                        let _ = app_output.emit("terminal-output", payload);
                    }
                    Err(err) => {
                        tracing::warn!("Terminal reader error for session {id}: {err}");
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
    cmd
}
