mod fixture;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fixture::Fixture;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::json;

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
config="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ "${CLAUDE_SECURESTORAGE_CONFIG_DIR+set}" = set ]; then
  credentials="${CLAUDE_SECURESTORAGE_CONFIG_DIR:-$HOME/.claude}"
else
  credentials="$config"
fi
mkdir -p "$config/plans" || exit 90
printf 'plan\n' > "$config/plans/dev-instance-plan.md" || exit 91
printf 'CLAUDE_PLAN_FILE=%s\n' "$config/plans/dev-instance-plan.md"
printf 'CLAUDE_CREDENTIAL=%s\n' "$(cat "$credentials/.credentials.json" 2>/dev/null)"
exit 3
"#;

fn free_loopback_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve debug MCP port");
    listener.local_addr().expect("debug MCP addr")
}

// Nextest isolates each test in its own process, so the process environment
// below belongs to this test alone. It is set before any runtime starts.
#[test]
fn dev_instance_claude_writes_stay_out_of_parent_claude_home() {
    let tools = tempfile::tempdir().expect("fake tool dir");
    let bin = tools.path().join("bin");
    std::fs::create_dir(&bin).expect("fake bin dir");
    executable(&bin.join("cargo-tauri"), FAKE_CARGO_TAURI);
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
        std::env::set_var("SHELL", &shell);
        std::env::set_var("CLAUDE_CONFIG_DIR", &parent_claude_home);
        std::env::remove_var("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    }
    server::process_env::initialize_process_env().expect("resolve login PATH");

    let error = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(start_dev_instance_expecting_exit());

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
            .any(|line| line == format!("CLAUDE_CREDENTIAL={CREDENTIAL_SENTINEL}")),
        "dev instance Claude must still read the parent's credentials"
    );
}

async fn start_dev_instance_expecting_exit() -> String {
    let debug_mcp_addr = free_loopback_addr();
    let _fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        debug_mcp_bind_addr: Some(debug_mcp_addr),
        ..Default::default()
    })
    .await;
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("server crate has a repo root")
        .to_path_buf();
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{debug_mcp_addr}/mcp"));
    let service = ().serve(transport).await.expect("connect to debug MCP");
    let result = service
        .call_tool(CallToolRequestParams {
            meta: None,
            name: "tyde_dev_instance_start".into(),
            arguments: json!({ "project_dir": repo_root.display().to_string() })
                .as_object()
                .cloned(),
            task: None,
        })
        .await
        .expect("call tyde_dev_instance_start");
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
