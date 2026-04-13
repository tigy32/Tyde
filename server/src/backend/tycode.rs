use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use protocol::{
    AgentInput, ChatEvent, ChatMessage, MessageSender, StreamEndData, StreamStartData,
    StreamTextDeltaData,
};

use super::{Backend, BackendSpawnConfig, EventStream};

const BACKEND_INPUT_BUFFER: usize = 64;
const BACKEND_EVENT_BUFFER: usize = 256;

/// Binary name for the tycode subprocess.
/// Can be overridden via the `TYDE_REMOTE_SUBPROCESS_PATH` env var.
fn subprocess_bin() -> String {
    std::env::var("TYDE_REMOTE_SUBPROCESS_PATH").unwrap_or_else(|_| "tycode-subprocess".into())
}

pub struct TycodeBackend {
    input_tx: mpsc::Sender<AgentInput>,
}

impl Backend for TycodeBackend {
    async fn spawn(
        workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::channel::<AgentInput>(BACKEND_INPUT_BUFFER);
        let (events_tx, events_rx) = mpsc::channel::<ChatEvent>(BACKEND_EVENT_BUFFER);
        let workspace_roots = if workspace_roots.is_empty() {
            vec!["/tmp".to_string()]
        } else {
            workspace_roots
        };

        tokio::spawn(async move {
            let roots_json = serde_json::json!(workspace_roots).to_string();

            let mut child = match Command::new(subprocess_bin())
                .arg("--workspace-roots")
                .arg(&roots_json)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess: {err}");
                    return;
                }
            };

            let mut stdin = match child.stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin");
                    return;
                }
            };
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout");
                    return;
                }
            };

            // Spawn a task to forward follow-up messages to stdin
            let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(BACKEND_INPUT_BUFFER);
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(message) = stdin_rx.recv().await {
                    if !write_command(&mut stdin, &message).await {
                        break;
                    }
                }
            });

            // Forward AgentInput to the stdin writer
            let stdin_tx2 = stdin_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            if stdin_tx2.send(payload.message).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });

            // Read stdout line by line — the subprocess emits ChatEvent JSON directly
            let mut lines = BufReader::new(stdout).lines();
            let mut stream_open = false;
            let mut accumulated_text = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let event: ChatEvent = match serde_json::from_str(trimmed) {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to parse tycode-subprocess event: {err} — line: {trimmed}"
                        );
                        continue;
                    }
                };

                match &event {
                    ChatEvent::StreamStart(StreamStartData { .. }) => {
                        stream_open = true;
                        accumulated_text.clear();
                    }
                    ChatEvent::StreamDelta(StreamTextDeltaData { text, .. }) => {
                        if stream_open {
                            accumulated_text.push_str(text);
                        }
                    }
                    ChatEvent::StreamEnd(_) => {
                        stream_open = false;
                    }
                    _ => {}
                }

                if events_tx.send(event).await.is_err() {
                    break;
                }
            }

            // Some tycode builds terminate without emitting StreamEnd. Synthesize
            // one so downstream callers don't hang waiting for end-of-turn.
            if stream_open {
                let _ = events_tx
                    .send(ChatEvent::StreamEnd(StreamEndData {
                        message: ChatMessage {
                            timestamp: unix_now_ms(),
                            sender: MessageSender::Assistant {
                                agent: "tycode".to_string(),
                            },
                            content: accumulated_text,
                            reasoning: None,
                            tool_calls: Vec::new(),
                            model_info: None,
                            token_usage: None,
                            context_breakdown: None,
                            images: None,
                        },
                    }))
                    .await;
            }
        });

        Ok((Self { input_tx }, EventStream::new(events_rx)))
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).await.is_ok()
    }
}

/// Write a send_message command to the subprocess stdin.
async fn write_command(stdin: &mut tokio::process::ChildStdin, message: &str) -> bool {
    let payload = serde_json::json!({
        "UserInput": message,
    });

    let line = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("Failed to serialize tycode command: {err}");
            return false;
        }
    };

    if let Err(err) = stdin.write_all(line.as_bytes()).await {
        tracing::error!("Failed to write to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.write_all(b"\n").await {
        tracing::error!("Failed to write newline to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess stdin: {err}");
        return false;
    }
    true
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
