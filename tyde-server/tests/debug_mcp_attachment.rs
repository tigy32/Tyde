//! An owned `tyde-server host --uds` publishes the `tyde-debug` attachment a
//! backend would receive, announces it on stdout, serves the DEV launcher
//! through it, retracts it on shutdown, and refuses a stale file or an empty
//! attachment path.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::json;

fn owned_host(state: &Path, attachment: &Path, stderr: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tyde-server"));
    command
        .args(["host", "--uds"])
        .env("HOME", state)
        .env("TMPDIR", state)
        .env("TYDE_SOCKET_PATH", state.join("tyde.sock"))
        .env("TYDE_DEBUG_MCP_ATTACHMENT_PATH", attachment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(stderr).expect("create host log"));
    command
}

/// Kills the host if the test fails before the graceful shutdown it asserts.
struct OwnedHost(Option<Child>);

impl OwnedHost {
    fn terminate(mut self) -> ExitStatus {
        let mut host = self.0.take().expect("owned host");
        let terminated = Command::new("kill")
            .args(["-TERM", &host.id().to_string()])
            .status()
            .expect("signal owned host");
        assert!(terminated.success());
        host.wait().expect("join owned host")
    }
}

impl Drop for OwnedHost {
    fn drop(&mut self) {
        if let Some(mut host) = self.0.take() {
            let _ = host.kill();
            let _ = host.wait();
        }
    }
}

fn refused(attachment: &Path, refusal: &str) {
    let state = tempfile::tempdir().expect("refused host state");
    let log = state.path().join("host.log");
    let output = owned_host(state.path(), attachment, &log)
        .output()
        .expect("run refused host");
    assert!(!output.status.success(), "{refusal}: host must refuse");
    assert!(
        output.stdout.is_empty(),
        "{refusal}: nothing may be announced"
    );
    let reason = std::fs::read_to_string(&log).expect("refused host log");
    assert!(
        reason.contains(refusal),
        "refusal must say {refusal:?}: {reason}"
    );
}

#[test]
fn owned_host_publishes_and_retracts_debug_mcp_attachment() {
    let state = tempfile::tempdir().expect("owned host state");
    let attachment = state.path().join("debug-mcp.json");
    let log = state.path().join("host.log");
    let mut child = owned_host(state.path(), &attachment, &log)
        .spawn()
        .expect("start owned host");
    let mut stdout = BufReader::new(child.stdout.take().expect("host stdout"));
    let host = OwnedHost(Some(child));
    let mut announcement = String::new();
    stdout
        .read_line(&mut announcement)
        .expect("read host announcement");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&announcement).unwrap_or_else(|error| panic!(
            "host announcement {announcement:?} is not JSON ({error}); log: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        )),
        json!({ "debug_mcp_attachment": attachment.to_str().expect("UTF-8 path") }),
    );
    let mode = std::fs::metadata(&attachment)
        .expect("announced attachment exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "attachment must be owner-only");
    let descriptor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&attachment).expect("read attachment"))
            .expect("attachment is JSON");
    let servers = descriptor["servers"].as_array().expect("servers");
    assert_eq!(servers.len(), 1, "{descriptor}");
    assert_eq!(servers[0]["name"], "tyde-debug");
    assert_eq!(servers[0]["transport"]["kind"], "http");
    assert_eq!(servers[0]["transport"]["headers"], json!({}));
    assert_eq!(descriptor["supports_parallel_tool_calls"], false);
    let url = servers[0]["transport"]["url"]
        .as_str()
        .expect("attachment URL")
        .to_owned();
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with("/mcp"),
        "debug MCP must stay loopback: {url}"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("MCP client runtime");
    runtime.block_on(async {
        let client =
            ().serve(StreamableHttpClientTransport::from_uri(url))
                .await
                .expect("connect through attachment");
        let tools: Vec<String> = client
            .list_all_tools()
            .await
            .expect("list attached tools")
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        for required in [
            "tyde_dev_instance_start",
            "tyde_dev_instance_wait",
            "tyde_dev_instance_list",
            "tyde_dev_instance_stop",
        ] {
            assert!(
                tools.iter().any(|tool| tool == required),
                "{required} missing from {tools:?}"
            );
        }
        let inventory = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "tyde_dev_instance_list".into(),
                arguments: Some(serde_json::Map::new()),
                task: None,
            })
            .await
            .expect("list DEV instances through attachment");
        assert_ne!(inventory.is_error, Some(true), "{inventory:?}");
        let RawContent::Text(text) = &inventory.content.first().expect("inventory").raw else {
            panic!("DEV inventory must be JSON text");
        };
        let inventory: serde_json::Value =
            serde_json::from_str(&text.text).expect("DEV inventory JSON");
        assert_eq!(
            inventory,
            json!([]),
            "a fresh owned host owns no DEV attempts"
        );
        client.cancel().await.expect("close MCP client");
    });

    let exit = host.terminate();
    assert!(
        exit.success(),
        "owned host must shut down gracefully: {exit}; log: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(!attachment.exists(), "shutdown must retract the attachment");

    let stale_state = tempfile::tempdir().expect("stale attachment state");
    let stale = stale_state.path().join("debug-mcp.json");
    std::fs::write(&stale, "previous host").expect("stale attachment");
    refused(&stale, &stale.display().to_string());
    assert_eq!(
        std::fs::read_to_string(&stale).expect("stale attachment kept"),
        "previous host"
    );
    refused(
        Path::new(""),
        "TYDE_DEBUG_MCP_ATTACHMENT_PATH is set but empty",
    );
}
