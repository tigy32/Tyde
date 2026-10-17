mod fixture;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fixture::Fixture;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CREDENTIAL_SENTINEL: &str = "parent-claude-credential";

fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("mark executable");
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

// Resolves Claude's config and credential directories with the Claude CLI's
// own rules: CLAUDE_CONFIG_DIR (default ~/.claude) holds plans, transcripts,
// and state; CLAUDE_SECURESTORAGE_CONFIG_DIR, when set, holds credentials
// (empty means ~/.claude), otherwise credentials live in the config dir.
const FAKE_CARGO_TAURI: &str = r#"#!/bin/sh
# Starting this CLI must follow frontend readiness, not initiate it.
/usr/bin/python3 - "$@" <<'PYCONFIG' || exit 89
import json,pathlib,sys,os
path=sys.argv[sys.argv.index('--config')+1]
assert json.load(open(path))['build']['beforeDevCommand'] is None
pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER']).touch()
PYCONFIG
config="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ "${CLAUDE_SECURESTORAGE_CONFIG_DIR+set}" = set ]; then
  credentials="${CLAUDE_SECURESTORAGE_CONFIG_DIR:-$HOME/.claude}"
else
  credentials="$config"
fi
mkdir -p "$config/plans" || exit 90
printf 'plan\n' > "$config/plans/dev-instance-plan.md" || exit 91
printf 'CLAUDE_PLAN_FILE=%s\n' "$config/plans/dev-instance-plan.md"
[ "$(cat "$credentials/.credentials.json" 2>/dev/null)" = "$TYDE_TEST_CLAUDE_CREDENTIAL" ] || exit 92
printf 'CLAUDE_CREDENTIAL_MATCH=1\n'
exit 3
"#;

const FAKE_NODE: &str = r#"#!/usr/bin/python3
import http.server,os,socket,sys
assert sys.argv[1].endswith('/tools/trunk-command.mjs')
assert sys.argv[2]=='serve' and '--no-autoreload' in sys.argv
port=int(sys.argv[sys.argv.index('--port')+1])
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL'])
control.sendall(b'B')
assert control.recv(1)==b'R'
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200);self.end_headers();self.wfile.write(b'<html>fixture</html>')
    def log_message(self,*args):pass
http.server.HTTPServer(('127.0.0.1',port),Handler).serve_forever()
"#;

// Nextest isolates each test in its own process, so the process environment
// below belongs to this test alone. It is set before any runtime starts.
#[test]
fn dev_instance_claude_writes_stay_out_of_parent_claude_home() {
    let tools = tempfile::tempdir().expect("fake tool dir");
    let bin = tools.path().join("bin");
    std::fs::create_dir(&bin).expect("fake bin dir");
    executable(&bin.join("cargo-tauri"), FAKE_CARGO_TAURI);
    executable(&bin.join("node"), FAKE_NODE);
    let control_dir = tempfile::Builder::new()
        .prefix("tyde-front-")
        .tempdir_in("/tmp")
        .expect("short frontend control directory");
    let control_path = control_dir.path().join("control");
    let desktop_marker = control_dir.path().join("desktop-started");
    let original_path = std::env::var_os("PATH").expect("test PATH");
    let resolved_path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .expect("fake login PATH");
    let shell = tools.path().join("login-shell");
    executable(
        &shell,
        &format!(
            "#!/bin/sh\nprintf 'TYDE_SHELL_PROBE_BEGIN_7f3c9a2e=%s=TYDE_SHELL_PROBE_END_7f3c9a2e\\n' {}\n",
            shell_quote(Path::new(&resolved_path)),
        ),
    );
    let parent_claude_home = tools.path().join("parent-claude");
    std::fs::create_dir(&parent_claude_home).expect("parent Claude home");
    std::fs::write(
        parent_claude_home.join(".credentials.json"),
        CREDENTIAL_SENTINEL,
    )
    .expect("parent Claude credentials");
    unsafe {
        std::env::set_var("TYDE_TEST_FRONTEND_CONTROL", &control_path);
        std::env::set_var("TYDE_TEST_DESKTOP_MARKER", &desktop_marker);
        std::env::set_var("SHELL", &shell);
        std::env::set_var("CLAUDE_CONFIG_DIR", &parent_claude_home);
        std::env::set_var("TYDE_TEST_CLAUDE_CREDENTIAL", CREDENTIAL_SENTINEL);
        std::env::remove_var("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
    server::process_env::initialize_process_env().expect("resolve login PATH");

    let error = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(start_dev_instance_expecting_exit(
            &control_path,
            &desktop_marker,
        ));

    assert!(
        error.contains("exited before ready"),
        "failed startup must report child exit and captured output"
    );
    let plan_file = error
        .lines()
        .find_map(|line| line.strip_prefix("CLAUDE_PLAN_FILE="))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("dev instance did not report its plan file: {error}"));
    assert!(
        !plan_file.starts_with(&parent_claude_home),
        "dev instance Claude plans must not land in the parent Claude home"
    );
    assert!(
        plan_file.starts_with(std::env::temp_dir()),
        "dev instance Claude plans must land in its ephemeral store"
    );
    assert!(
        !parent_claude_home.join("plans").exists(),
        "dev instance wrote into the parent Claude home"
    );
    assert!(
        !plan_file.exists(),
        "failed dev instance start must remove its isolated Claude home"
    );
    assert!(
        error
            .lines()
            .any(|line| line == "CLAUDE_CREDENTIAL_MATCH=1"),
        "dev instance Claude must still read the parent's credentials"
    );
}

async fn start_dev_instance_expecting_exit(control_path: &Path, desktop_marker: &Path) -> String {
    let control =
        tokio::net::UnixListener::bind(control_path).expect("bind frontend readiness gate");
    // Let the server retain its ephemeral listener: reserving then releasing a
    // port raced other tests and failed with AddressAlreadyInUse.
    let fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        debug_mcp_bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    })
    .await;
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("nextest must provide the current package directory");
    let repo_root = Path::new(&manifest_dir)
        .parent()
        .expect("server crate has a repo root")
        .to_path_buf();
    eprintln!(
        "DEV INSTANCE test paths: runtime_root_exists={}",
        repo_root.is_dir()
    );
    let transport = StreamableHttpClientTransport::from_uri(
        fixture.host_for_test().debug_mcp_url_for_test().await,
    );
    let service = ().serve(transport).await.expect("connect to debug MCP");
    let mut start = Box::pin(
        service.call_tool(CallToolRequestParams {
            meta: None,
            name: "tyde_dev_instance_start".into(),
            arguments: json!({ "project_dir": repo_root.display().to_string() })
                .as_object()
                .cloned(),
            task: None,
        }),
    );
    let (mut control, _) = tokio::select! {
        result = &mut start => panic!("dev start settled before frontend readiness gate: tool_completed={}", result.is_ok()),
        connected = control.accept() => connected.expect("frontend entered actual readiness gate"),
    };
    assert_eq!(
        control.read_u8().await.expect("frontend control gate"),
        b'B'
    );
    assert!(
        !desktop_marker.exists(),
        "desktop started before frontend was ready"
    );
    control
        .write_all(b"R")
        .await
        .expect("release actual frontend readiness");
    let result = start.as_mut().await.expect("call tyde_dev_instance_start");
    drop(start);
    assert!(
        desktop_marker.exists(),
        "desktop did not start after frontend became ready"
    );
    let mut byte = [0];
    assert_eq!(
        control
            .read(&mut byte)
            .await
            .expect("frontend exit observation"),
        0,
        "failed dev start must stop its owned frontend"
    );
    service.cancel().await.expect("cancel debug MCP client");
    assert_eq!(
        result.is_error,
        Some(true),
        "fake Tauri CLI exits, so the start must fail: {result:?}"
    );
    let content = result.content.first().expect("start error content");
    let RawContent::Text(text) = &content.raw else {
        panic!("expected text start error, got {:?}", content.raw);
    };
    text.text.clone()
}
