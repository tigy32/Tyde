use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use devtools_protocol::{
    UiDebugRequest, UiDebugRequestEvent, UiDebugResponse, UiDebugResponseSubmission,
};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, sleep, timeout};
use uuid::Uuid;

const UI_DEBUG_BIND_ENV: &str = "TYDE_DEV_UI_DEBUG_BIND_ADDR";
const UI_DEBUG_DEFAULT_TIMEOUT_MS: u64 = 5_000;

#[derive(Default)]
pub struct UiDebugBridgeState {
    ready: AtomicBool,
    pending: Mutex<HashMap<String, oneshot::Sender<UiDebugResponse>>>,
}

impl UiDebugBridgeState {
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub async fn dispatch_request(
        &self,
        app: &AppHandle,
        request: UiDebugRequest,
    ) -> Result<UiDebugResponse, String> {
        let timeout_ms = request_timeout_ms(&request);
        self.wait_until_ready(timeout_ms).await?;

        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let previous = self.pending.lock().await.insert(request_id.clone(), tx);
        assert!(
            previous.is_none(),
            "duplicate pending UI debug request_id {request_id}"
        );

        let event = UiDebugRequestEvent {
            request_id: request_id.clone(),
            request,
        };
        if let Err(err) = app.emit("tyde://ui-debug-request", event) {
            self.pending.lock().await.remove(&request_id);
            return Err(format!("failed to emit UI debug request: {err}"));
        }

        match timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err("UI debug frontend dropped the response channel".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(format!("UI debug request timed out after {timeout_ms}ms"))
            }
        }
    }

    pub async fn submit_response(
        &self,
        submission: UiDebugResponseSubmission,
    ) -> Result<(), String> {
        let tx = self.pending.lock().await.remove(&submission.request_id);
        let Some(tx) = tx else {
            return Err(format!(
                "unknown UI debug request_id '{}'",
                submission.request_id
            ));
        };

        tx.send(submission.response)
            .map_err(|_| "failed to deliver UI debug response".to_string())
    }

    async fn wait_until_ready(&self, timeout_ms: u64) -> Result<(), String> {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.ready.load(Ordering::Acquire) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("UI debug frontend is not ready".to_string());
            }
            sleep(Duration::from_millis(25)).await;
        }
    }
}

pub fn start_ui_debug_http_server(
    app: &tauri::AppHandle,
    bridge: Arc<UiDebugBridgeState>,
) -> Result<Option<String>, String> {
    let bind_addr = resolve_bind_addr_from_env()?.unwrap_or_else(default_bind_addr);
    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind UI debug listener at {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set nonblocking UI debug listener: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to resolve UI debug listener addr: {err}"))?;

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!("failed to create async UI debug listener: {err}");
                return;
            }
        };

        loop {
            let (stream, remote_addr) = match listener.accept().await {
                Ok(parts) => parts,
                Err(err) => {
                    tracing::error!("UI debug listener accept failed: {err}");
                    break;
                }
            };

            if !remote_addr.ip().is_loopback() {
                tracing::warn!("rejecting non-loopback UI debug client {remote_addr}");
                continue;
            }

            let app = app.clone();
            let bridge = bridge.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = handle_client(app, bridge, stream).await {
                    tracing::warn!("UI debug request failed: {err}");
                }
            });
        }
    });

    Ok(Some(local_addr.to_string()))
}

async fn handle_client(
    app: AppHandle,
    bridge: Arc<UiDebugBridgeState>,
    mut stream: tokio::net::TcpStream,
) -> Result<(), String> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .map_err(|err| format!("failed to read UI debug request: {err}"))?;
    if bytes.is_empty() {
        return Err("UI debug client sent an empty request".to_string());
    }

    let request: UiDebugRequest = serde_json::from_slice(&bytes)
        .map_err(|err| format!("failed to parse UI debug request JSON: {err}"))?;
    let response = match request {
        UiDebugRequest::CaptureScreenshot { .. } => UiDebugResponse::Error {
            message: "capture_screenshot is not implemented yet".to_string(),
        },
        other => bridge
            .dispatch_request(&app, other)
            .await
            .unwrap_or_else(|message| UiDebugResponse::Error { message }),
    };
    let body = serde_json::to_vec(&response)
        .map_err(|err| format!("failed to serialize UI debug response JSON: {err}"))?;
    stream
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write UI debug response: {err}"))?;
    Ok(())
}

fn request_timeout_ms(request: &UiDebugRequest) -> u64 {
    match request {
        UiDebugRequest::Evaluate { timeout_ms, .. } => {
            timeout_ms.unwrap_or(UI_DEBUG_DEFAULT_TIMEOUT_MS)
        }
        UiDebugRequest::Ping | UiDebugRequest::CaptureScreenshot { .. } => {
            UI_DEBUG_DEFAULT_TIMEOUT_MS
        }
    }
}

fn resolve_bind_addr_from_env() -> Result<Option<SocketAddr>, String> {
    let raw = match std::env::var(UI_DEBUG_BIND_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => return Err(format!("failed to read {UI_DEBUG_BIND_ENV}: {err}")),
    };

    let addr = raw
        .parse::<SocketAddr>()
        .map_err(|err| format!("invalid {UI_DEBUG_BIND_ENV}='{raw}': {err}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "non-loopback {UI_DEBUG_BIND_ENV} is not allowed: {addr}"
        ));
    }

    Ok(Some(addr))
}

fn default_bind_addr() -> SocketAddr {
    "127.0.0.1:0"
        .parse()
        .expect("default UI debug bind addr must parse")
}
