mod bridge;
mod router;

use router::ProxyRouterHandle;
use tauri::Manager;

struct ShellState {
    router: ProxyRouterHandle,
    host: server::HostHandle,
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
            app.manage(ShellState { router, host });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connect_host,
            disconnect_host,
            send_host_line
        ])
        .run(tauri::generate_context!())
        .expect("failed to run desktop shell");
}
