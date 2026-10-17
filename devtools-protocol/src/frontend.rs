use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tyde_process_env::launch::{AsyncGroupChild, GroupCommand};

pub fn command(repo_root: &Path, store_dir: &Path, port: u16) -> Result<GroupCommand, String> {
    let mut command = tyde_process_env::command("node")?;
    command
        .arg(repo_root.join("tools/trunk-command.mjs"))
        .args(["serve", "--address", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .arg("--no-autoreload")
        // Trunk serves even after an initial build error. An isolated dist
        // prevents an old index from certifying a failed new build as ready.
        .arg("--dist")
        .arg(store_dir.join("frontend-dist"))
        .current_dir(repo_root);
    Ok(command)
}

pub async fn wait_until_ready(child: &mut AsyncGroupChild, port: u16) -> Result<(), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let ready = async {
        loop {
            let stream = match tokio::net::TcpStream::connect(address).await {
                Ok(stream) => stream,
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    // Retry connection refusal while the owned build is alive,
                    // without imposing a deadline on healthy compilation.
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                Err(error) => return Err(format!("frontend connection failed: {error}")),
            };
            let mut stream = BufReader::new(stream);
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await
                .map_err(|error| format!("frontend readiness request failed: {error}"))?;
            let mut status = String::new();
            stream
                .take(1024)
                .read_line(&mut status)
                .await
                .map_err(|error| format!("frontend readiness response failed: {error}"))?;
            if !matches!(status.split_whitespace().nth(1), Some("200")) {
                return Err(
                    "frontend initial build did not serve its index successfully".to_owned(),
                );
            }
            return Ok(());
        }
    };
    tokio::select! {
        biased;
        status = child.wait() => match status {
            Ok(status) => Err(format!("dev frontend exited before ready: {status}")),
            Err(error) => Err(format!("dev frontend process wait failed: {error}")),
        },
        result = ready => result,
    }
}
