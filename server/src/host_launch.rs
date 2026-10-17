//! Explicit managed-host handoff and startup acknowledgement, without a shell.
#![cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
const STARTUP_SOCKET: &str = "TYDE_HOST_STARTUP_SOCKET";

#[cfg(unix)]
pub fn take_notification() -> Option<PathBuf> {
    let path = std::env::var_os(STARTUP_SOCKET).map(PathBuf::from);
    // Entrypoints call this before creating runtime threads or child probes.
    unsafe { std::env::remove_var(STARTUP_SOCKET) };
    path
}
#[cfg(unix)]
pub fn notify_ready(path: Option<PathBuf>) -> Result<(), String> {
    use std::io::Write;
    let Some(path) = path else {
        return Ok(());
    };
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(mut stream) => stream
            .write_all(b"TYHR")
            .map_err(|error| format!("host startup notification failed: {error}")),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            // Lifetime was already transferred explicitly before target exec;
            // a departed launcher does not revoke the running service.
            tracing::info!(error_kind=?error.kind(),"managed-host startup receiver departed after handoff");
            Ok(())
        }
        Err(error) => Err(format!(
            "host startup notification connection failed: {error}"
        )),
    }
}

#[cfg(unix)]
pub fn launch_current(program: &Path, log_path: &Path, managed: bool) -> Result<(), String> {
    let current =
        std::env::current_exe().map_err(|error| format!("locate host executable: {error}"))?;
    let resolved = std::fs::canonicalize(program)
        .map_err(|error| format!("resolve host executable identity: {error}"))?;
    if resolved != current {
        return Err(
            "host launcher executable does not identify the running Tyde binary".to_owned(),
        );
    }
    let directory = tempfile::Builder::new()
        .prefix("tyde-start-")
        .tempdir_in("/tmp")
        .map_err(|error| format!("create private host startup directory: {error}"))?;
    let socket = directory.path().join("ready");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create host launch runtime: {error}"))?;
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path)
        .map_err(|error| format!("open host launch log: {error}"))?;
    let error_log = log
        .try_clone()
        .map_err(|error| format!("duplicate host launch log: {error}"))?;
    runtime.block_on(async move {
        use tokio::io::AsyncReadExt;
        use crate::process_env::launch::{IndependentProcess,ProcessStdio};
        let listener=tokio::net::UnixListener::bind(&socket)
            .map_err(|error|format!("bind private host startup socket: {error}"))?;
        let mut command=crate::process_env::command(program)?;
        command.args(["host","--uds"]).env(STARTUP_SOCKET,&socket)
            .stdin(ProcessStdio::Null).stdout(log).stderr(error_log);
        if managed {command.arg("--managed");}
        let mut child=command.launch_independent(IndependentProcess::ManagedHost).await
            .map_err(|error|format!("launch host: {error}"))?;
        tokio::select! {
            result=async {
                let (mut stream,_)=listener.accept().await?;
                let mut marker=[0;4];
                stream.read_exact(&mut marker).await?;
                if marker != *b"TYHR" {return Err(std::io::Error::other("invalid host startup acknowledgement"));}
                Ok::<(),std::io::Error>(())
            }=>result.map_err(|error|format!("receive host startup acknowledgement: {error}")),
            result=child.wait()=>match result {
                Ok(status)=>Err(format!("host exited before startup acknowledgement: {status}")),
                Err(error)=>Err(format!("host failed before startup acknowledgement: {error}")),
            },
        }
    })
}
