use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
            let mut socket = tokio::net::UnixStream::connect(&socket_path)
                .await
                .map_err(|err| {
                    format!(
                        "failed to connect to Tyde UDS host at {}: {err}",
                        socket_path.display()
                    )
                })?;
            let mut stdio = StdioTransport::new();
            tokio::io::copy_bidirectional(&mut stdio, &mut socket)
                .await
                .map_err(|err| format!("UDS bridge transport failed: {err}"))?;
            Ok(())
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

struct StdioTransport {
    stdin: tokio::io::Stdin,
    stdout: tokio::io::Stdout,
}

impl StdioTransport {
    fn new() -> Self {
        Self {
            stdin: tokio::io::stdin(),
            stdout: tokio::io::stdout(),
        }
    }
}

impl AsyncRead for StdioTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_read(cx, buf)
    }
}

impl AsyncWrite for StdioTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.stdout).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdout).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stdout).poll_shutdown(cx)
    }
}
