use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::backend::{StartupMcpServer, StartupMcpTransport};
const ACP_DEFAULT_FILE_LINE_LIMIT: usize = 2_000;
const ACP_DEFAULT_TERMINAL_OUTPUT_LIMIT: usize = 1_048_576;

#[derive(Clone)]
pub enum AcpInbound {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Stderr(String),
    Closed {
        exit_code: Option<i32>,
    },
}

#[derive(Clone)]
pub struct AcpSpawnSpec {
    pub display_name: String,
    pub local_program: String,
    pub local_args: Vec<String>,
    pub remote_args: Vec<String>,
    pub local_cwd: Option<String>,
    pub remote_cwd: Option<String>,
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
}

pub struct AcpBridge {
    rpc: AcpRpc,
    terminals: Mutex<HashMap<String, Arc<Mutex<AcpTerminal>>>>,
    next_terminal_id: AtomicU64,
}

impl AcpBridge {
    pub fn spawn(
        spec: AcpSpawnSpec,
        ssh_host: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AcpInbound>), String> {
        let (rpc, inbound_rx) = AcpRpc::spawn(spec, ssh_host)?;
        Ok((
            Self {
                rpc,
                terminals: Mutex::new(HashMap::new()),
                next_terminal_id: AtomicU64::new(1),
            },
            inbound_rx,
        ))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc.request(method, params).await
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.rpc.notify(method, params).await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.rpc.respond(id, result).await
    }

    pub async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), String> {
        self.rpc.respond_error(id, code, message).await
    }

    pub async fn handle_server_request(
        &self,
        id: Value,
        method: &str,
        params: &Value,
    ) -> Result<bool, String> {
        let result = match self.handle_builtin_request(method, params).await {
            Ok(Some(result)) => result,
            Ok(None) => return Ok(false),
            Err(err) => {
                self.rpc.respond_error(id, -32_000, &err).await?;
                return Ok(true);
            }
        };

        self.rpc.respond(id, result).await?;
        Ok(true)
    }

    pub async fn shutdown(&self) {
        let terminals = {
            let mut map = self.terminals.lock().await;
            map.drain()
                .map(|(_, terminal)| terminal)
                .collect::<Vec<_>>()
        };
        for terminal in terminals {
            let _ = terminate_terminal(terminal).await;
        }
        self.rpc.shutdown().await;
    }

    async fn handle_builtin_request(
        &self,
        method: &str,
        params: &Value,
    ) -> Result<Option<Value>, String> {
        match method {
            "fs/read_text_file" => self.handle_fs_read_text_file(params).await.map(Some),
            "fs/write_text_file" => self.handle_fs_write_text_file(params).await.map(Some),
            "terminal/create" => self.handle_terminal_create(params).await.map(Some),
            "terminal/output" => self.handle_terminal_output(params).await.map(Some),
            "terminal/wait_for_exit" => self.handle_terminal_wait_for_exit(params).await.map(Some),
            "terminal/kill" => self.handle_terminal_kill(params).await.map(Some),
            "terminal/release" => self.handle_terminal_release(params).await.map(Some),
            "session/request_permission" => self.handle_request_permission(params).await.map(Some),
            _ => Ok(None),
        }
    }

    async fn handle_fs_read_text_file(&self, params: &Value) -> Result<Value, String> {
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

        let raw_bytes = tokio::fs::read(path)
            .await
            .map_err(|err| format!("Failed to read file '{path}': {err}"))?;
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

    async fn handle_fs_write_text_file(&self, params: &Value) -> Result<Value, String> {
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

        if let Some(parent) = Path::new(path).parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| format!("Failed to create parent directory for '{path}': {err}"))?;
        }

        tokio::fs::write(path, content)
            .await
            .map_err(|err| format!("Failed to write file '{path}': {err}"))?;

        Ok(Value::Null)
    }

    async fn handle_terminal_create(&self, params: &Value) -> Result<Value, String> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|cmd| !cmd.is_empty())
            .ok_or("terminal/create requires non-empty 'command'")?;

        let args = params
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

        if let Some(path) = cwd.as_ref() {
            if !Path::new(path).is_absolute() {
                return Err("terminal/create requires absolute 'cwd'".to_string());
            }
        }

        let output_limit = params
            .get("outputByteLimit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(ACP_DEFAULT_TERMINAL_OUTPUT_LIMIT)
            .max(1);

        let mut cmd = Command::new(command);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(path) = cwd.as_ref() {
            cmd.current_dir(path);
        }
        for (name, value) in env_vars {
            cmd.env(name, value);
        }

        let mut child = cmd
            .spawn()
            .map_err(|err| format!("Failed to spawn terminal command '{command}': {err}"))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let terminal = Arc::new(Mutex::new(AcpTerminal {
            child: Some(child),
            output: Vec::new(),
            output_limit,
            truncated: false,
            exit_code: None,
            exit_signal: None,
        }));

        if let Some(reader) = stdout {
            tokio::spawn(read_terminal_output(reader, Arc::clone(&terminal)));
        }
        if let Some(reader) = stderr {
            tokio::spawn(read_terminal_output(reader, Arc::clone(&terminal)));
        }

        let terminal_id = format!(
            "term-{}",
            self.next_terminal_id.fetch_add(1, Ordering::Relaxed)
        );
        self.terminals
            .lock()
            .await
            .insert(terminal_id.clone(), terminal);

        Ok(json!({ "terminalId": terminal_id }))
    }

    async fn handle_terminal_output(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .lock()
            .await
            .get(&terminal_id)
            .cloned()
            .ok_or_else(|| format!("Unknown terminalId '{terminal_id}'"))?;

        refresh_terminal_status(&terminal).await;

        let guard = terminal.lock().await;
        let output = String::from_utf8_lossy(&guard.output).to_string();
        let exit_status = match (&guard.exit_code, &guard.exit_signal) {
            (Some(exit_code), signal) => Some(json!({ "exitCode": exit_code, "signal": signal })),
            (None, Some(signal)) => Some(json!({ "exitCode": Value::Null, "signal": signal })),
            (None, None) => None,
        };

        let mut payload = json!({
            "output": output,
            "truncated": guard.truncated,
        });
        if let Some(status) = exit_status {
            payload["exitStatus"] = status;
        }
        Ok(payload)
    }

    async fn handle_terminal_wait_for_exit(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .lock()
            .await
            .get(&terminal_id)
            .cloned()
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
            .lock()
            .await
            .get(&terminal_id)
            .cloned()
            .ok_or_else(|| format!("Unknown terminalId '{terminal_id}'"))?;

        kill_terminal(terminal).await?;
        Ok(Value::Null)
    }

    async fn handle_terminal_release(&self, params: &Value) -> Result<Value, String> {
        let terminal_id = terminal_id_from_params(params)?;
        let terminal = self
            .terminals
            .lock()
            .await
            .remove(&terminal_id)
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
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                return parsed;
            }
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

    let success = status == "completed" || status == "success";
    let tool_result = params
        .get("rawOutput")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

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
    })
}

pub fn extract_text_from_update(update: &Value) -> String {
    if let Some(text) = update.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            return text.to_string();
        }
    }

    if let Some(delta) = update.get("delta").and_then(Value::as_str) {
        if !delta.is_empty() {
            return delta.to_string();
        }
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

    if let Some(content_type) = content.get("type").and_then(Value::as_str) {
        if content_type == "content" {
            if let Some(inner) = content.get("content") {
                return extract_text_from_content(inner);
            }
        }
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

struct AcpTerminal {
    child: Option<Child>,
    output: Vec<u8>,
    output_limit: usize,
    truncated: bool,
    exit_code: Option<i32>,
    exit_signal: Option<String>,
}

async fn read_terminal_output<R>(mut reader: R, terminal: Arc<Mutex<AcpTerminal>>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        let read = match reader.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(err) => {
                let mut guard = terminal.lock().await;
                append_output_to_terminal(
                    &mut guard,
                    format!("\n[terminal stream read error: {err}]\n").as_bytes(),
                );
                return;
            }
        };

        let mut guard = terminal.lock().await;
        append_output_to_terminal(&mut guard, &buf[..read]);
    }
}

fn append_output_to_terminal(terminal: &mut AcpTerminal, chunk: &[u8]) {
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

async fn refresh_terminal_status(terminal: &Arc<Mutex<AcpTerminal>>) {
    let status = {
        let mut guard = terminal.lock().await;
        if guard.exit_code.is_some() || guard.exit_signal.is_some() {
            return;
        }

        let Some(child) = guard.child.as_mut() else {
            return;
        };

        child.try_wait().unwrap_or_default()
    };

    if let Some(status) = status {
        let (exit_code, signal) = exit_status_pair(&status);
        let mut guard = terminal.lock().await;
        guard.exit_code = exit_code;
        guard.exit_signal = signal;
        guard.child = None;
    }
}

async fn wait_for_terminal_exit(
    terminal: Arc<Mutex<AcpTerminal>>,
) -> Result<(Option<i32>, Option<String>), String> {
    let child_opt = {
        let mut guard = terminal.lock().await;
        if guard.exit_code.is_some() || guard.exit_signal.is_some() {
            return Ok((guard.exit_code, guard.exit_signal.clone()));
        }
        guard.child.take()
    };

    if let Some(mut child) = child_opt {
        let status = child
            .wait()
            .await
            .map_err(|err| format!("Failed to wait for terminal process: {err}"))?;
        let (exit_code, signal) = exit_status_pair(&status);
        let mut guard = terminal.lock().await;
        guard.exit_code = exit_code;
        guard.exit_signal = signal.clone();
        return Ok((exit_code, signal));
    }

    let guard = terminal.lock().await;
    Ok((guard.exit_code, guard.exit_signal.clone()))
}

async fn kill_terminal(terminal: Arc<Mutex<AcpTerminal>>) -> Result<(), String> {
    let child_opt = {
        let mut guard = terminal.lock().await;
        if guard.exit_code.is_some() || guard.exit_signal.is_some() {
            return Ok(());
        }
        guard.child.take()
    };

    if let Some(mut child) = child_opt {
        child
            .kill()
            .await
            .map_err(|err| format!("Failed to kill terminal process: {err}"))?;
        let status = child
            .wait()
            .await
            .map_err(|err| format!("Failed to wait for killed terminal process: {err}"))?;
        let (exit_code, signal) = exit_status_pair(&status);
        let mut guard = terminal.lock().await;
        guard.exit_code = exit_code;
        guard.exit_signal = signal;
    }

    Ok(())
}

async fn terminate_terminal(terminal: Arc<Mutex<AcpTerminal>>) -> Result<(), String> {
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

type PendingRpcMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct AcpRpc {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingRpcMap,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<Child>>>,
}

impl AcpRpc {
    fn spawn(
        spec: AcpSpawnSpec,
        ssh_host: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AcpInbound>), String> {
        let mut child = if let Some(host) = ssh_host {
            use crate::remote::shell_quote_command;

            let remote_exec = format!(
                "PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\" {}",
                shell_quote_command(&spec.remote_args),
            );
            let remote_cmd = if let Some(cwd) = spec
                .remote_cwd
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                format!(
                    "cd {} && {}",
                    crate::remote::shell_quote_arg(cwd),
                    remote_exec
                )
            } else {
                remote_exec
            };

            let mut cmd = Command::new("ssh");
            for arg in crate::remote::ssh_control_args()? {
                cmd.arg(arg);
            }
            cmd.arg("-T")
                .arg(host)
                .arg(remote_cmd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|err| format!("Failed to spawn {} over SSH: {err}", spec.display_name))?
        } else {
            let mut cmd = Command::new(&spec.local_program);
            cmd.args(&spec.local_args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(path) = spec
                .local_cwd
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                cmd.current_dir(path);
            }
            cmd.spawn()
                .map_err(|err| format!("Failed to spawn {}: {err}", spec.display_name))?
        };

        let stdin = child.stdin.take().ok_or("Failed to capture ACP stdin")?;
        let stdout = child.stdout.take().ok_or("Failed to capture ACP stdout")?;
        let stderr = child.stderr.take().ok_or("Failed to capture ACP stderr")?;

        let child_ref = Arc::new(Mutex::new(Some(child)));
        let pending: PendingRpcMap = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();

        let stdout_pending = Arc::clone(&pending);
        let stdout_inbound = inbound_tx.clone();
        let stdout_child = Arc::clone(&child_ref);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let parsed = match serde_json::from_str::<Value>(&line) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!("Failed to parse ACP stdout JSON: {err}; line: {line}");
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

                            Err(message)
                        };

                        if let Some(tx) = stdout_pending.lock().await.remove(&id) {
                            let _ = tx.send(response);
                        }
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
                        });
                    } else {
                        let _ = stdout_inbound.send(AcpInbound::Notification {
                            method: method.to_string(),
                            params,
                        });
                    }
                }
            }

            let exit_code = match stdout_child.lock().await.as_mut() {
                Some(child) => child
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|status| status.code()),
                None => None,
            };

            let mut pending = stdout_pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("ACP process exited before response".to_string()));
            }
            drop(pending);

            let _ = stdout_inbound.send(AcpInbound::Closed { exit_code });
        });

        let stderr_inbound = inbound_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_inbound.send(AcpInbound::Stderr(line));
            }
        });

        Ok((
            Self {
                stdin: Arc::new(Mutex::new(stdin)),
                pending,
                next_id: AtomicU64::new(1),
                child: child_ref,
            },
            inbound_rx,
        ))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self.send_json(&payload).await {
            let _ = self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err("ACP response channel closed".to_string()),
        }
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
        let mut stdin = self.stdin.lock().await;
        let line = format!("{value}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| format!("Failed to write to ACP stdin: {err}"))
    }

    async fn shutdown(&self) {
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = child.start_kill();
        }
    }
}
