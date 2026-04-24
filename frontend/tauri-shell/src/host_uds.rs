use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

        let socket_path = resolve_socket_path()?;
        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        let log_dir = PathBuf::from(home).join(".tyde").join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|err| {
            format!(
                "failed to create Tyde log directory {}: {err}",
                log_dir.display()
            )
        })?;
        let log_path = log_dir.join("tyde-host.log");
        let exe = std::env::current_exe()
            .map_err(|err| format!("failed to locate current Tyde executable: {err}"))?;
        let command = format!(
            "nohup {} host --uds >> {} 2>&1 < /dev/null &",
            shell_quote_path(&exe),
            shell_quote_path(&log_path)
        );
        let status = std::process::Command::new("sh")
            .arg("-lc")
            .arg(command)
            .status()
            .map_err(|err| format!("failed to launch Tyde UDS host: {err}"))?;
        if !status.success() {
            return Err(format!(
                "failed to launch Tyde UDS host: shell exited with {status}"
            ));
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "Tyde UDS host did not become reachable at {} within 5s; see {}",
                    socket_path.display(),
                    log_path.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(unix)]
fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
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
