use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex as SyncMutex;
use serde::Serialize;
use serde_json::Value;

use crate::AppState;

/// A running Tyde dev instance spawned via `tyde_dev_instance_start`.
pub(crate) struct DevInstance {
    /// Handle to the `npx tauri dev` child process tree.
    child: tokio::process::Child,
    /// Shared proxy state (cheap to clone for use across await points).
    proxy: Arc<McpProxy>,
    /// Project directory the dev instance was launched from.
    #[allow(dead_code)]
    pub(crate) project_dir: String,
}

/// Lightweight MCP client for proxying tool calls to the dev instance.
struct McpProxy {
    debug_mcp_url: String,
    http_client: reqwest::Client,
    session_id: SyncMutex<Option<String>>,
    rpc_id: AtomicU64,
}

#[derive(Debug, Serialize)]
pub(crate) struct DevInstanceStartResult {
    pub(crate) debug_mcp_url: String,
    pub(crate) status: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct DevInstanceStopResult {
    pub(crate) status: &'static str,
}

impl McpProxy {
    /// Proxy an MCP `tools/call` request to the dev instance's debug MCP server.
    /// Returns the `result` field from the JSON-RPC response.
    async fn tool_call(&self, tool_name: &str, arguments: Value) -> Result<Value, String> {
        self.ensure_initialized().await?;

        let id = self.rpc_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            }
        });

        let mut request = self
            .http_client
            .post(&self.debug_mcp_url)
            .header("Accept", "application/json, text/event-stream")
            .json(&body);

        if let Some(sid) = self.session_id.lock().as_ref() {
            request = request.header("Mcp-Session-Id", sid);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("Failed to proxy to dev instance: {e}"))?;

        self.capture_session_id(&response);

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("Failed to read dev instance response: {e}"))?;

        if !status.is_success() {
            return Err(format!("Dev instance returned HTTP {status}: {text}"));
        }

        let json_text = Self::extract_json_from_response(&text)?;

        let rpc_response: Value = serde_json::from_str(&json_text)
            .map_err(|e| format!("Failed to parse dev instance JSON-RPC response: {e}"))?;

        if let Some(error) = rpc_response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("Dev instance MCP error: {message}"));
        }

        rpc_response
            .get("result")
            .cloned()
            .ok_or_else(|| "Dev instance response missing 'result' field".to_string())
    }

    /// Ensure the MCP session has been initialized with the dev instance.
    async fn ensure_initialized(&self) -> Result<(), String> {
        if self.session_id.lock().is_some() {
            return Ok(());
        }

        let id = self.rpc_id.fetch_add(1, Ordering::Relaxed);
        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "tyde-dev-proxy",
                    "version": "0.1.0"
                }
            }
        });

        let response = self
            .http_client
            .post(&self.debug_mcp_url)
            .header("Accept", "application/json, text/event-stream")
            .json(&init_body)
            .send()
            .await
            .map_err(|e| format!("Failed to initialize MCP session with dev instance: {e}"))?;

        self.capture_session_id(&response);

        // Read and discard the body so the connection can be reused.
        let _ = response.text().await;

        // Send the `initialized` notification.
        let sid = self.session_id.lock().clone();
        if let Some(sid) = sid {
            let notif_body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            });
            let _ = self
                .http_client
                .post(&self.debug_mcp_url)
                .header("Accept", "application/json, text/event-stream")
                .header("Mcp-Session-Id", &sid)
                .json(&notif_body)
                .send()
                .await;
        }

        Ok(())
    }

    fn capture_session_id(&self, response: &reqwest::Response) {
        if let Some(sid) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock() = Some(sid.to_string());
        }
    }

    /// The Streamable HTTP transport may return SSE (text/event-stream) or
    /// plain JSON. Extract the JSON-RPC payload from either format.
    fn extract_json_from_response(text: &str) -> Result<String, String> {
        let trimmed = text.trim();
        if trimmed.starts_with("data:") || trimmed.contains("event: message") {
            trimmed
                .lines()
                .find(|line| line.starts_with("data: ") || line.starts_with("data:"))
                .map(|line| line.trim_start_matches("data:").trim().to_string())
                .ok_or_else(|| "Dev instance SSE response missing data line".to_string())
        } else {
            Ok(text.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start a new Tyde dev instance.
pub(crate) async fn start_dev_instance(
    state: &AppState,
    project_dir: String,
) -> Result<DevInstanceStartResult, String> {
    {
        let guard = state.dev_instance.lock();
        if guard.is_some() {
            return Err(
                "A dev instance is already running. Stop it first with tyde_dev_instance_stop."
                    .to_string(),
            );
        }
    }

    // Find available ports by binding to :0 and reading the assigned ports.
    // We need two: one for the debug MCP server, one for Vite (to avoid
    // colliding with the host's Vite on 5173).
    let find_free_port = || -> Result<u16, String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("Failed to find available port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local addr: {e}"))?
            .port();
        Ok(port)
    };
    let debug_mcp_port = find_free_port()?;
    let vite_port = find_free_port()?;

    let debug_mcp_url = format!("http://127.0.0.1:{debug_mcp_port}/mcp");

    // Override tauri.conf.json devUrl to point at the Vite port we chose,
    // using Tauri CLI's --config JSON merge patch.
    let tauri_config_override = format!(
        r#"{{"build":{{"devUrl":"http://localhost:{vite_port}"}}}}"#
    );

    // Spawn `npx tauri dev` with env vars that configure the child instance:
    // - TYDE_VITE_PORT: Vite dev server port (read by vite.config.ts)
    // - TYDE_DEBUG_MCP_HTTP_BIND_ADDR: debug MCP server bind address
    // - TYDE_DEBUG_MCP_HTTP_ENABLED: enable debug MCP in the child
    // - TYDE_MCP_HTTP_ENABLED=false: disable agent control MCP (recursion prevention)
    // - TYDE_DRIVER_MCP_HTTP_ENABLED=false: disable driver MCP (recursion prevention)
    let mut child = tokio::process::Command::new("npx")
        .args(["tauri", "dev", "--config", &tauri_config_override])
        .current_dir(&project_dir)
        .env("TYDE_VITE_PORT", vite_port.to_string())
        .env(
            "TYDE_DEBUG_MCP_HTTP_BIND_ADDR",
            format!("127.0.0.1:{debug_mcp_port}"),
        )
        .env("TYDE_DEBUG_MCP_HTTP_ENABLED", "true")
        .env("TYDE_MCP_HTTP_ENABLED", "false")
        .env("TYDE_DRIVER_MCP_HTTP_ENABLED", "false")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn `npx tauri dev`: {e}"))?;

    // Poll healthz until the dev instance's debug MCP server is ready.
    // Check if the child process has exited on each iteration so we fail
    // fast instead of polling a dead port for 5 minutes.
    let healthz_url = format!("http://127.0.0.1:{debug_mcp_port}/healthz");
    let client = reqwest::Client::new();
    let poll_timeout = std::time::Duration::from_secs(300);
    let poll_interval = std::time::Duration::from_secs(2);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > poll_timeout {
            let _ = child.kill().await;
            return Err(format!(
                "Dev instance did not become ready within {}s. \
                 The build may have failed — check the project for compilation errors.",
                poll_timeout.as_secs()
            ));
        }

        if let Some(exit_status) = child
            .try_wait()
            .map_err(|e| format!("Failed to check dev instance process: {e}"))?
        {
            return Err(format!(
                "Dev instance process exited before becoming ready (exit status: {exit_status}). \
                 The build likely failed — check the project for compilation errors."
            ));
        }

        match client.get(&healthz_url).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => {}
        }

        tokio::time::sleep(poll_interval).await;
    }

    let proxy = Arc::new(McpProxy {
        debug_mcp_url: debug_mcp_url.clone(),
        http_client: client,
        session_id: SyncMutex::new(None),
        rpc_id: AtomicU64::new(1),
    });

    let instance = DevInstance {
        child,
        proxy,
        project_dir: project_dir.clone(),
    };

    *state.dev_instance.lock() = Some(instance);

    tracing::info!("Dev instance started: {debug_mcp_url} (project: {project_dir})");

    Ok(DevInstanceStartResult {
        debug_mcp_url,
        status: "running",
    })
}

/// Stop the running dev instance.
pub(crate) async fn stop_dev_instance(state: &AppState) -> Result<DevInstanceStopResult, String> {
    let mut instance = state
        .dev_instance
        .lock()
        .take()
        .ok_or_else(|| "No dev instance is running.".to_string())?;

    if let Err(e) = instance.child.kill().await {
        tracing::warn!("Failed to kill dev instance process: {e}");
    }

    tracing::info!(
        "Dev instance stopped (was: {})",
        instance.proxy.debug_mcp_url
    );

    Ok(DevInstanceStopResult { status: "stopped" })
}

/// Proxy an MCP tool call to the running dev instance.
/// Returns an error if no dev instance is running.
pub(crate) async fn proxy_debug_tool_call(
    state: &AppState,
    tool_name: &str,
    arguments: Value,
) -> Result<Value, String> {
    // Clone the Arc out of the lock so we don't hold SyncMutex across await.
    let proxy = {
        let guard = state.dev_instance.lock();
        let instance = guard.as_ref().ok_or_else(|| {
            "No dev instance running. Call tyde_dev_instance_start first.".to_string()
        })?;
        Arc::clone(&instance.proxy)
    };
    proxy.tool_call(tool_name, arguments).await
}
