use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

            server::serve_uds(listener, server::ServerConfig::current(), host)
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
        let log_dir = server::paths::home_dir()?.join(".tyde").join("logs");
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

    Ok(server::paths::home_dir()?.join(".tyde").join("tyde.sock"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::bind_host_socket_before_start;
    use std::cell::Cell;

    #[tokio::test]
    async fn socket_contention_rejects_before_host_start() {
        let path =
            std::path::PathBuf::from(format!("/tmp/tyde-uds-test-{}.sock", uuid::Uuid::new_v4()));
        let owner = server::bind_uds(&path).await.expect("first socket owner");
        let started = Cell::new(false);

        let result = bind_host_socket_before_start(&path, || {
            started.set(true);
            Ok(())
        })
        .await;

        assert!(
            result.is_err(),
            "the second starter must lose at the socket"
        );
        assert!(!started.get(), "a socket loser must not construct a host");
        drop(owner);
        assert!(!path.exists(), "the socket owner must clean up its path");
    }
}
