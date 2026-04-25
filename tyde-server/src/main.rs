use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

const HOST_SOCKET_PATH_ENV: &str = "TYDE_SOCKET_PATH";

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliMode {
    HostStdio,
    HostUds,
    HostStatusUds,
    HostLaunchUds,
    HostBridgeUds,
    Version,
    Help,
    Error(String),
}

fn main() {
    match parse_cli_mode(std::env::args().skip(1)) {
        CliMode::HostStdio => exit_on_error(run_host_stdio()),
        CliMode::HostUds => exit_on_error(run_host_uds()),
        CliMode::HostStatusUds => exit_on_error(run_host_status_uds()),
        CliMode::HostLaunchUds => exit_on_error(run_host_launch_uds()),
        CliMode::HostBridgeUds => exit_on_error(run_host_bridge_uds()),
        CliMode::Version => println!("{}", env!("CARGO_PKG_VERSION")),
        CliMode::Help => print_usage(),
        CliMode::Error(message) => {
            eprintln!("ERROR: {message}");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    }
}

fn exit_on_error(result: Result<(), String>) {
    if let Err(err) = result {
        eprintln!("ERROR: {err}");
        std::process::exit(1);
    }
}

fn parse_cli_mode<I>(args: I) -> CliMode
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let args = args
        .into_iter()
        .map(Into::into)
        .filter(|arg| !arg.starts_with("-psn_"))
        .collect::<Vec<_>>();

    if args.is_empty() {
        return CliMode::Help;
    }

    if args.len() == 1 && matches!(args[0].as_str(), "-h" | "--help" | "help") {
        return CliMode::Help;
    }

    if args.len() == 1 && matches!(args[0].as_str(), "-V" | "--version" | "version") {
        return CliMode::Version;
    }

    if args.as_slice() == ["host", "--stdio"] || args.as_slice() == ["--stdio"] {
        return CliMode::HostStdio;
    }

    if args.as_slice() == ["host", "--uds"] || args.as_slice() == ["--uds"] {
        return CliMode::HostUds;
    }

    if args.as_slice() == ["host", "--status-uds"] || args.as_slice() == ["--status-uds"] {
        return CliMode::HostStatusUds;
    }

    if args.as_slice() == ["host", "--launch-uds"] || args.as_slice() == ["--launch-uds"] {
        return CliMode::HostLaunchUds;
    }

    if args.as_slice() == ["host", "--bridge-uds"] || args.as_slice() == ["--bridge-uds"] {
        return CliMode::HostBridgeUds;
    }

    match args.as_slice() {
        [host] if host == "host" => CliMode::Error(
            "missing transport for host mode; use `tyde-server host --stdio`, `tyde-server host --uds`, `tyde-server host --status-uds`, `tyde-server host --launch-uds`, or `tyde-server host --bridge-uds`"
                .to_owned(),
        ),
        _ => CliMode::Error(format!("unknown arguments: {}", args.join(" "))),
    }
}

fn print_usage() {
    println!("Usage:");
    println!("  tyde-server --version          Print the Tyde server binary version");
    println!("  tyde-server host --stdio       Run a Tyde host over stdin/stdout");
    println!("  tyde-server host --uds         Run a Tyde host over ~/.tyde/tyde.sock");
    println!("  tyde-server host --status-uds  Check whether the Tyde UDS host is reachable");
    println!("  tyde-server host --launch-uds  Launch the Tyde UDS host in the background");
    println!("  tyde-server host --bridge-uds  Bridge stdin/stdout to a running Tyde UDS host");
}

fn init_logging() -> Result<(), String> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_line_number(true);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .try_init()
        .map_err(|err| format!("failed to initialize tracing subscriber: {err}"))
}

fn run_host_stdio() -> Result<(), String> {
    init_logging()?;
    tracing::info!("starting tyde-server stdio mode");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to create tokio runtime for host stdio mode: {err}"))?;

    runtime.block_on(async {
        let host = spawn_host()?;
        let transport = StdioTransport::new();
        let connection = server::accept(&server::ServerConfig::current(), transport)
            .await
            .map_err(|err| format!("host stdio handshake failed: {err:?}"))?;

        server::run_connection(connection, host)
            .await
            .map_err(|err| format!("host stdio connection failed: {err:?}"))
    })
}

fn run_host_uds() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        init_logging()?;
        let socket_path = resolve_socket_path()?;
        tracing::info!("starting tyde-server UDS mode at {}", socket_path.display());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("failed to create tokio runtime for host UDS mode: {err}"))?;

        runtime.block_on(async move {
            let host = spawn_host()?;
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

fn run_host_status_uds() -> Result<(), String> {
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

fn run_host_launch_uds() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS launch mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        if run_host_status_uds().is_ok() {
            return Ok(());
        }

        let socket_path = resolve_socket_path()?;
        let home = home_dir()?;
        let log_dir = home.join(".tyde").join("logs");
        std::fs::create_dir_all(&log_dir).map_err(|err| {
            format!(
                "failed to create Tyde log directory {}: {err}",
                log_dir.display()
            )
        })?;
        let log_path = log_dir.join("tyde-server-host.log");
        let exe = std::env::current_exe()
            .map_err(|err| format!("failed to locate current tyde-server executable: {err}"))?;
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

fn run_host_bridge_uds() -> Result<(), String> {
    #[cfg(not(unix))]
    {
        return Err("host UDS bridge mode requires Unix domain sockets".to_string());
    }

    #[cfg(unix)]
    {
        init_logging()?;
        let socket_path = resolve_socket_path()?;
        tracing::info!(
            "starting tyde-server UDS bridge mode for {}",
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

fn spawn_host() -> Result<server::HostHandle, String> {
    server::spawn_host_with_store_paths_and_runtime_config(
        server::store::session::SessionStore::default_path()?,
        server::store::project::ProjectStore::default_path()?,
        server::store::settings::HostSettingsStore::default_path()?,
        server::HostRuntimeConfig::default(),
    )
}

#[cfg(unix)]
fn resolve_socket_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(HOST_SOCKET_PATH_ENV) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    Ok(home_dir()?.join(".tyde").join("tyde.sock"))
}

#[cfg(unix)]
fn home_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
    Ok(PathBuf::from(home))
}

#[cfg(unix)]
fn shell_quote_path(path: &std::path::Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
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

#[cfg(test)]
mod tests {
    use super::{CliMode, parse_cli_mode};

    #[test]
    fn parses_host_modes() {
        assert_eq!(
            parse_cli_mode(vec!["host".to_string(), "--stdio".to_string()]),
            CliMode::HostStdio
        );
        assert_eq!(
            parse_cli_mode(vec!["host".to_string(), "--uds".to_string()]),
            CliMode::HostUds
        );
        assert_eq!(
            parse_cli_mode(vec!["host".to_string(), "--bridge-uds".to_string()]),
            CliMode::HostBridgeUds
        );
    }

    #[test]
    fn parses_version() {
        assert_eq!(
            parse_cli_mode(vec!["--version".to_string()]),
            CliMode::Version
        );
    }
}
