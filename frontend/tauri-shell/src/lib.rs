mod bridge;
mod dev_host;
mod devtools;
mod router;

use std::sync::Arc;

use devtools_protocol::UiDebugResponseSubmission;
use router::ProxyRouterHandle;
use tauri::Manager;

struct ShellState {
    router: ProxyRouterHandle,
    host: server::HostHandle,
    ui_debug: Arc<devtools::UiDebugBridgeState>,
}

#[tauri::command]
async fn connect_host(
    app: tauri::AppHandle,
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<(), String> {
    state
        .router
        .connect_local(app, host_id, state.host.clone())
        .await
}

#[tauri::command]
async fn disconnect_host(
    state: tauri::State<'_, ShellState>,
    host_id: String,
) -> Result<(), String> {
    state.router.disconnect(host_id).await
}

#[tauri::command]
async fn send_host_line(
    state: tauri::State<'_, ShellState>,
    host_id: String,
    line: String,
) -> Result<(), String> {
    state.router.send_line(host_id, line).await
}

#[tauri::command]
fn mark_ui_debug_ready(state: tauri::State<'_, ShellState>) {
    state.ui_debug.mark_ready();
}

#[tauri::command]
async fn submit_ui_debug_response(
    state: tauri::State<'_, ShellState>,
    request_id: String,
    response: devtools_protocol::UiDebugResponse,
) -> Result<(), String> {
    state
        .ui_debug
        .submit_response(UiDebugResponseSubmission {
            request_id,
            response,
        })
        .await
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("starting tyde shell");

    tauri::Builder::default()
        .setup(|app| {
            tracing::info!("setup: spawning host and router");
            let host = server::spawn_host();
            let router = ProxyRouterHandle::new();
            let ui_debug = Arc::new(devtools::UiDebugBridgeState::default());

            if let Some(addr) =
                dev_host::start_dev_host_listener(host.clone()).map_err(std::io::Error::other)?
            {
                tracing::info!("dev host listener ready at {addr}");
            }
            if let Some(url) = devtools::start_ui_debug_http_server(app.handle(), ui_debug.clone())
                .map_err(std::io::Error::other)?
            {
                tracing::info!("ui debug HTTP server ready at {url}");
            }

            app.manage(ShellState {
                router,
                host,
                ui_debug,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect_host,
            disconnect_host,
            send_host_line,
            mark_ui_debug_ready,
            submit_ui_debug_response
        ])
        .run(tauri::generate_context!())
        .expect("failed to run desktop shell");
}
