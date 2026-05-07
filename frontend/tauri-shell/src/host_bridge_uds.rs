use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const HOST_SOCKET_PATH_ENV: &str = "TYDE_SOCKET_PATH";

pub fn run() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS bridge mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        if let Err(err) = super::logging::init_host_bridge_uds_logging() {
            eprintln!("warning: failed to initialize host UDS bridge logging: {err}");
        }

        install_parent_death_watch();

        let socket_path = resolve_socket_path()?;
        tracing::info!(
            "starting tyde host UDS bridge mode for {}",
            socket_path.display()
        );

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                format!("failed to create tokio runtime for host UDS bridge mode: {err}")
            })?;

        runtime.block_on(async move {
            let socket = tokio::net::UnixStream::connect(&socket_path)
                .await
                .map_err(|err| {
                    format!(
                        "failed to connect to Tyde UDS host at {}: {err}",
                        socket_path.display()
                    )
                })?;

            let (socket_read, socket_write) = socket.into_split();
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();

            let cancel = CancellationToken::new();

            let stdin_to_socket = {
                let cancel = cancel.clone();
                tokio::spawn(copy_loop("stdin->uds", stdin, socket_write, cancel))
            };
            let socket_to_stdout = {
                let cancel = cancel.clone();
                tokio::spawn(copy_loop("uds->stdout", socket_read, stdout, cancel))
            };

            let mut stdin_to_socket = stdin_to_socket;
            let mut socket_to_stdout = socket_to_stdout;
            let result = tokio::select! {
                res = &mut stdin_to_socket => {
                    cancel.cancel();
                    socket_to_stdout.abort();
                    res.unwrap_or_else(|err| Err(format!("stdin->uds task join error: {err}")))
                }
                res = &mut socket_to_stdout => {
                    cancel.cancel();
                    stdin_to_socket.abort();
                    res.unwrap_or_else(|err| Err(format!("uds->stdout task join error: {err}")))
                }
            };

            let _ = stdin_to_socket.await;
            let _ = socket_to_stdout.await;

            result
        })
    }
}

#[cfg(unix)]
async fn copy_loop<R, W>(
    label: &'static str,
    mut reader: R,
    mut writer: W,
    cancel: CancellationToken,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            read = reader.read(&mut buf) => {
                match read {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        if let Err(err) = writer.write_all(&buf[..n]).await {
                            return Err(format!("{label} write failed: {err}"));
                        }
                        if let Err(err) = writer.flush().await {
                            return Err(format!("{label} flush failed: {err}"));
                        }
                    }
                    Err(err) => return Err(format!("{label} read failed: {err}")),
                }
            }
        }
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

#[cfg(target_os = "linux")]
fn install_parent_death_watch() {
    // Ask the kernel to deliver SIGTERM when our parent dies, then double-check
    // whether we were already reparented to init before prctl was installed.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
    let ppid = unsafe { libc::getppid() };
    if ppid == 1 {
        std::process::exit(0);
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn install_parent_death_watch() {
    // No PR_SET_PDEATHSIG equivalent here. A reparented bridge relies on SSH
    // closing stdin/stdout to unblock copy_loop.
}

#[cfg(not(unix))]
fn install_parent_death_watch() {}
