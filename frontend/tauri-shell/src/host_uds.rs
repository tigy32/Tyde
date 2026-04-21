use std::path::PathBuf;

const HOST_SOCKET_PATH_ENV: &str = "TYDE_SOCKET_PATH";

pub fn run() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        if let Err(err) = super::logging::init_host_uds_logging() {
            eprintln!("warning: failed to initialize host UDS logging: {err}");
        }

        let socket_path = resolve_socket_path()?;
        tracing::info!("starting tyde host UDS mode at {}", socket_path.display());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to create tokio runtime for host UDS mode: {err}"))?;

        runtime.block_on(async move {
            let host = server::spawn_host_with_store_paths_and_runtime_config(
                server::store::session::SessionStore::default_path()?,
                server::store::project::ProjectStore::default_path()?,
                server::store::settings::HostSettingsStore::default_path()?,
                server::HostRuntimeConfig::default(),
            )?;

            server::listen_uds(&socket_path, server::ServerConfig::current(), host)
                .await
                .map_err(|err| {
                    format!(
                        "host UDS listener failed at {}: {err}",
                        socket_path.display()
                    )
                })
        })
    }
}

#[cfg(unix)]
fn resolve_socket_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(HOST_SOCKET_PATH_ENV) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
    Ok(PathBuf::from(home).join(".tyde").join("tyde.sock"))
}
