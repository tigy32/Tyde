use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc, oneshot, watch},
};

use crate::backend::subprocess::AsyncCommandGroup;

const FRAME_LIMIT: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub(super) struct RpcError {
    pub code: i64,
    message: String,
    #[serde(default)]
    pub data: Value,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.data.get("kind").and_then(Value::as_str);
        let retained_envelope_missing_id = self.message.contains("retained")
            && self.message.contains("missing field")
            && self.message.contains("`id`");
        let rejection_reason = match self.data.get("reason").and_then(Value::as_str) {
            Some("invalid_goal_state") => "invalid_goal_state",
            Some("runtime_busy") => "runtime_busy",
            Some("goal_control_unavailable") => "goal_control_unavailable",
            _ => "unclassified",
        };
        tracing::debug!(
            rejection_reason,
            code = self.code,
            retained_envelope_missing_id,
            "Muse RPC error classification"
        );
        write!(
            f,
            "Muse RPC rejected the operation: {} (code {}, retained-envelope-missing-id: {})",
            kind.unwrap_or("unclassified"),
            self.code,
            retained_envelope_missing_id
        )
    }
}

pub(super) enum Incoming {
    Notification { method: String, params: Value },
    Failed(String),
}

struct Request {
    method: &'static str,
    params: Value,
    reply: oneshot::Sender<Result<Value, RpcError>>,
    notification: bool,
}

struct Lifetime {
    commands: mpsc::UnboundedSender<Request>,
    stop: Arc<Notify>,
    reaped: watch::Receiver<Option<Result<(), String>>>,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

#[derive(Clone)]
pub(super) struct Connection(Arc<Lifetime>);

impl Connection {
    pub async fn spawn(
        program: &str,
        args: &[String],
        customization: Option<tempfile::TempDir>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Incoming>), String> {
        let mut command = crate::backend::subprocess::command(program)?;
        command
            .args(args)
            .stdin(crate::backend::subprocess::ProcessStdio::Piped)
            .stdout(crate::backend::subprocess::ProcessStdio::Piped)
            .stderr(crate::backend::subprocess::ProcessStdio::Piped);
        let mut child = command
            .group_spawn()
            .await
            .map_err(|error| format!("Cannot start Muse serve: {error}"))?;
        let stdin = child.take_stdin().ok_or("Muse stdin was not captured")?;
        let stdout = child.take_stdout().ok_or("Muse stdout was not captured")?;
        let stderr = child.take_stderr().ok_or("Muse stderr was not captured")?;
        let (commands, mut command_rx) = mpsc::unbounded_channel::<Request>();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let stop = Arc::new(Notify::new());
        let stop_task = stop.clone();
        let (reaped_tx, reaped) = watch::channel(None);
        tokio::spawn(async move {
            let mut stdin = stdin;
            let mut output = BufReader::new(stdout);
            let mut errors = BufReader::new(stderr).lines();
            let mut error_stream_open = true;
            let mut pending = HashMap::new();
            let mut sequence = 0_u64;
            let mut line = Vec::new();
            let failure = loop {
                tokio::select! {
                    biased;
                    _ = stop_task.notified() => break None,
                    request = command_rx.recv() => {
                        let Some(request) = request else { break None; };
                        sequence += 1;
                        let mut frame = json!({"jsonrpc":"2.0", "method":request.method, "params":request.params});
                        if !request.notification { frame["id"] = json!(sequence); }
                        let mut bytes = match serde_json::to_vec(&frame) {
                            Ok(bytes) if bytes.len() <= FRAME_LIMIT => bytes,
                            Ok(_) => {
                                let _ = request.reply.send(Err(local_error("inputTooLarge")));
                                continue;
                            }
                            Err(_) => {
                                let _ = request.reply.send(Err(local_error("invalidParams")));
                                continue;
                            }
                        };
                        bytes.push(b'\n');
                        let presentation = if request.notification { Some(request.reply) } else {
                            pending.insert(sequence, request.reply);
                            None
                        };
                        tokio::select! {
                            biased;
                            _ = stop_task.notified() => break None,
                            result = stdin.write_all(&bytes) => {
                                if let Err(error) = result { break Some(format!("Muse input pipe failed: {error}")); }
                            }
                        }
                        if let Some(reply) = presentation { let _ = reply.send(Ok(json!({}))); }
                    }
                    result = async {
                        (&mut output).take((FRAME_LIMIT - line.len() + 1) as u64).read_until(b'\n', &mut line).await
                    } => {
                        match result {
                            Ok(0) => break Some("Muse closed its protocol output".to_owned()),
                            Ok(_) if line.len() > FRAME_LIMIT || line.last() != Some(&b'\n') => break Some("Muse emitted an oversized or unterminated protocol frame".to_owned()),
                            Err(error) => break Some(format!("Muse output pipe failed: {error}")),
                            Ok(_) => {}
                        }
                        let frame: Value = match serde_json::from_slice(&line) {
                            Ok(frame) => frame,
                            Err(error) => break Some(format!("Muse emitted invalid JSON: {error}")),
                        };
                        line.clear();
                        if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                            break Some("Muse emitted a non-MSP envelope".to_owned());
                        }
                        if let Some(method) = frame.get("method").and_then(Value::as_str) {
                            if let Some(id) = frame.get("id") {
                                // These replies acknowledge presentation, never approve or answer.
                                let result = match method {
                                    "approval/request" | "userInput/request" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
                                    _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported Muse server request"}}),
                                };
                                let mut bytes = match serde_json::to_vec(&result) {
                                    Ok(bytes) => bytes,
                                    Err(error) => break Some(format!("Cannot acknowledge Muse presentation: {error}")),
                                };
                                bytes.push(b'\n');
                                tokio::select! {
                                    biased;
                                    _ = stop_task.notified() => break None,
                                    result = stdin.write_all(&bytes) => {
                                        if let Err(error) = result { break Some(format!("Muse presentation pipe failed: {error}")); }
                                    }
                                }
                            } else {
                                let params = match frame.get("params") {
                                    Some(params) => params.clone(),
                                    None => json!({}),
                                };
                                if event_tx.send(Incoming::Notification { method: method.to_owned(), params }).is_err() { break None; }
                            }
                        } else {
                            let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                                break Some("Muse replied without a numeric request identity".to_owned());
                            };
                            let Some(reply) = pending.remove(&id) else {
                                break Some("Muse replied to an unknown or completed request".to_owned());
                            };
                            let result = match (frame.get("result"), frame.get("error")) {
                                (Some(result), None) => Ok(result.clone()),
                                (None, Some(error)) => match serde_json::from_value(error.clone()) {
                                    Ok(error) => Err(error),
                                    Err(_) => break Some("Muse emitted a malformed RPC rejection".to_owned()),
                                },
                                _ => break Some("Muse reply must carry exactly one result or error".to_owned()),
                            };
                            let _ = reply.send(result);
                        }
                    }
                    line = errors.next_line(), if error_stream_open => {
                        match line {
                            Ok(Some(line)) => tracing::debug!(bytes = line.len(), "Muse stderr activity"),
                            Ok(None) => error_stream_open = false,
                            Err(error) => break Some(format!("Muse stderr pipe failed: {error}")),
                        }
                    }
                }
            };
            for (_, reply) in pending {
                let _ = reply.send(Err(local_error("transportClosed")));
            }
            if let Some(failure) = failure {
                tracing::error!(message = failure, "Muse protocol transport terminated");
                let _ = event_tx.send(Incoming::Failed(failure));
            }
            if let Err(error) = child.start_kill() {
                tracing::error!(%error, "Muse process group could not be terminated");
            }
            let reaped = child
                .wait()
                .await
                .map(|_| ())
                .map_err(|error| format!("Muse process group could not be reaped: {error}"));
            if let Err(error) = &reaped {
                tracing::error!(%error, "Muse process group could not be reaped");
            }
            drop(customization);
            let _ = reaped_tx.send(Some(reaped));
        });
        Ok((
            Self(Arc::new(Lifetime {
                commands,
                stop,
                reaped,
            })),
            event_rx,
        ))
    }

    pub async fn request(&self, method: &'static str, params: Value) -> Result<Value, RpcError> {
        self.send(method, params, false).await
    }

    pub async fn notify(&self, method: &'static str, params: Value) -> Result<(), RpcError> {
        self.send(method, params, true).await.map(|_| ())
    }

    async fn send(
        &self,
        method: &'static str,
        params: Value,
        notification: bool,
    ) -> Result<Value, RpcError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .commands
            .send(Request {
                method,
                params,
                reply,
                notification,
            })
            .map_err(|_| local_error("transportClosed"))?;
        receive.await.map_err(|_| local_error("transportClosed"))?
    }

    pub async fn typed<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<T, String> {
        let result = self
            .request(method, params)
            .await
            .map_err(|error| format!("{method}: {error}"))?;
        serde_json::from_value(result)
            .map_err(|error| format!("Invalid Muse {method} result: {error}"))
    }

    pub fn shutdown(&self) {
        self.0.stop.notify_one();
    }

    pub async fn close(&self) -> Result<(), String> {
        self.shutdown();
        let mut reaped = self.0.reaped.clone();
        loop {
            if let Some(result) = reaped.borrow().clone() {
                return result;
            }
            reaped.changed().await.map_err(|_| {
                "Muse process reaper closed before reporting its terminal state".to_owned()
            })?;
        }
    }
}

fn local_error(kind: &'static str) -> RpcError {
    RpcError {
        code: -32603,
        message: "Local Muse transport failure".to_owned(),
        data: json!({"kind":kind}),
    }
}
