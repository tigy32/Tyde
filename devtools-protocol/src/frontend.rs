use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tyde_process_env::launch::{AsyncGroupChild, GroupCommand};

use crate::{DEV_INSTANCE_STARTUP_CONTRACT, DevFrontendAttestation, DevFrontendProtocol};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Notification {
    protocol: DevFrontendProtocol,
    event: Event,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    BuildFinished {
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    Serving {
        port: u16,
        identity: String,
        index_sha256: String,
    },
}

pub struct Ready {
    pub port: u16,
    pub attestation: DevFrontendAttestation,
}

pub fn command(repo_root: &Path, store_dir: &Path) -> Result<GroupCommand, String> {
    let mut command = tyde_process_env::command("node")?;
    command
        .arg(repo_root.join("tools/trunk-command.mjs"))
        .args(["dev-instance", "--dist"])
        .arg(store_dir.join("frontend-dist"))
        .current_dir(repo_root);
    Ok(command)
}

pub async fn wait_until_ready(child: &mut AsyncGroupChild) -> Result<Ready, String> {
    // Only the wrapper writes this pipe; compiler diagnostics go to stderr.
    let stdout = child
        .take_stdout()
        .ok_or_else(|| "DEV frontend startup pipe missing".to_owned())?;
    let mut events = BufReader::new(stdout);
    let mut built = false;
    loop {
        let mut line = String::new();
        let bytes = (&mut events)
            .take(4097)
            .read_line(&mut line)
            .await
            .map_err(|error| format!("DEV frontend startup pipe failed: {error}"))?;
        if bytes == 0 {
            let status = child
                .wait()
                .await
                .map_err(|error| format!("DEV frontend process wait failed: {error}"))?;
            return Err(format!("dev frontend exited before ready: {status}"));
        }
        if bytes > 4096 || !line.ends_with('\n') {
            return Err("Invalid DEV frontend startup notification length".to_owned());
        }
        let notification: Notification = serde_json::from_str(&line)
            .map_err(|_| "Invalid DEV frontend startup notification".to_owned())?;
        if notification.protocol != DEV_INSTANCE_STARTUP_CONTRACT.frontend {
            return Err("Unsupported DEV frontend startup protocol".to_owned());
        }
        match notification.event {
            Event::BuildFinished { exit_code, signal } if !built => {
                if exit_code != Some(0) || signal.is_some() {
                    return Err(format!(
                        "frontend initial build failed: exit_code={exit_code:?} signal={signal:?}"
                    ));
                }
                built = true;
                eprintln!("DEV frontend initial build completed successfully");
            }
            Event::Serving {
                port,
                identity,
                index_sha256,
            } if built && port != 0 => {
                verify_index(port, &identity, &index_sha256).await?;
                return Ok(Ready {
                    port,
                    attestation: DevFrontendAttestation {
                        protocol: notification.protocol,
                        index_sha256,
                    },
                });
            }
            _ => return Err("Out-of-order DEV frontend startup notification".to_owned()),
        }
    }
}

async fn verify_index(port: u16, identity: &str, index_sha256: &str) -> Result<(), String> {
    if [identity, index_sha256]
        .iter()
        .any(|value| value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err("Invalid DEV frontend serving attestation".to_owned());
    }
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|error| format!("owned frontend connection failed: {error}"))?;
    let mut stream = BufReader::new(stream);
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .map_err(|error| format!("owned frontend readiness request failed: {error}"))?;
    let mut headers = Vec::new();
    loop {
        let bytes = (&mut stream)
            .take(8193 - headers.len() as u64)
            .read_until(b'\n', &mut headers)
            .await
            .map_err(|error| format!("owned frontend response failed: {error}"))?;
        if headers.len() > 8192 || bytes == 0 {
            return Err("Invalid owned frontend HTTP headers".to_owned());
        }
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = std::str::from_utf8(&headers)
        .map_err(|_| "Invalid owned frontend HTTP header encoding".to_owned())?;
    let mut lines = headers.lines();
    if lines.next().and_then(|line| line.split_whitespace().nth(1)) != Some("200") {
        return Err("Owned frontend did not serve its built index (expected HTTP 200)".to_owned());
    }
    let mut identity_matches = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("x-tyde-frontend-identity")
        {
            identity_matches = value.trim() == identity;
        }
    }
    if !identity_matches {
        return Err("Frontend serving identity mismatch".to_owned());
    }
    let mut body = Vec::new();
    stream
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut body)
        .await
        .map_err(|error| format!("owned frontend index read failed: {error}"))?;
    if body.is_empty()
        || body.len() > 4 * 1024 * 1024
        || format!("{:x}", Sha256::digest(&body)) != index_sha256
    {
        return Err("Frontend built index digest mismatch".to_owned());
    }
    Ok(())
}
