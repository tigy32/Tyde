use std::path::{Path, PathBuf};

const HOST_SOCKET_PATH_ENV: &str = "TYDE_SOCKET_PATH";

#[cfg(unix)]
async fn bind_host_socket_before_start<T>(
    socket_path: &Path,
    start: impl FnOnce() -> Result<T, String>,
) -> Result<(server::BoundUdsListener, T), String> {
    let listener = server::bind_uds(socket_path).await.map_err(|err| {
        format!(
            "host UDS listener failed at {}: {err}",
            socket_path.display()
        )
    })?;
    let host = start()?;
    Ok((listener, host))
}

pub fn run() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        let notification = server::host_launch::take_notification();
        server::init_process_env()?;
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
            // Host construction starts mobile access, so socket ownership must
            // be settled first or two starters can split desktop and mobile.
            let (listener, host) = bind_host_socket_before_start(&socket_path, || {
                server::spawn_host_with_store_paths_and_runtime_config(
                    server::store::session::SessionStore::default_path()?,
                    server::store::project::ProjectStore::default_path()?,
                    server::store::settings::HostSettingsStore::default_path()?,
                    server::HostRuntimeConfig {
                        agents_view_preferences_primary: false,
                        ..server::HostRuntimeConfig::default()
                    },
                )
            })
            .await?;

            server::host_launch::notify_ready(notification)?;
            server::serve_uds(
                listener,
                server::ServerConfig::current(),
                host,
                server::host_shutdown_signal().map_err(|error| error.to_string())?,
            )
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

pub fn status() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS status mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        let socket_path = resolve_socket_path()?;
        std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| {
            format!(
                "Tyde UDS host is not reachable at {}: {err}",
                socket_path.display()
            )
        })?;
        Ok(())
    }
}

pub fn launch() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS launch mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        if status().is_ok() {
            return Ok(());
        }

        let log_dir = server::paths::home_dir()?.join(".tyde").join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|err| {
            format!(
                "failed to create Tyde log directory {}: {err}",
                log_dir.display()
            )
        })?;
        let log_path = log_dir.join("tyde-host.log");
        let program =
            std::env::current_exe().map_err(|error| format!("locate host executable: {error}"))?;
        server::host_launch::launch_current(&program, &log_path, false)
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

    Ok(server::paths::home_dir()?.join(".tyde").join("tyde.sock"))
}
