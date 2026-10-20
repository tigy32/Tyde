mod fixture;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fixture::{Fixture, next_frame_matching_on};
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
import json,pathlib,sys,os,socket
path=sys.argv[sys.argv.index('--config')+1]
assert json.load(open(path))['build']['beforeDevCommand'] is None
marker=pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER'])
marker.touch()
marker.with_suffix('.namespace').write_text(json.dumps({'logical_cwd':os.getcwd()=='/mnt/tyde-workspace','private_namespace':os.readlink('/proc/self/ns/mnt')!=os.environ['TYDE_TEST_HOST_NAMESPACE'] if pathlib.Path('/proc/self/ns/mnt').exists() else False}))
if marker.with_suffix('.hold').exists():
 control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
 control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL']);control.sendall(b'D')
 assert control.recv(1)==b'R'

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
import http.server,json,os,pathlib,socket,sys
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL'])
assert sys.argv[1].endswith('/tools/trunk-command.mjs')
assert sys.argv[2]=='serve' and '--no-autoreload' in sys.argv
if sys.argv[1].startswith('/mnt/tyde-workspace/'):
 assert os.getcwd()=='/mnt/tyde-workspace'
 assert os.readlink('/proc/self/ns/mnt')!=os.environ['TYDE_TEST_HOST_NAMESPACE']
port=int(sys.argv[sys.argv.index('--port')+1])
proof=pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER']).with_suffix('.frontend')
def record(phase):
    proof.write_text(json.dumps({'phase':phase,'port':port,'pid':os.getpid()}))
    print(f'DEV FRONTEND phase={phase} port={port}',flush=True)
record('launched')
ready=False
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        global ready
        if not ready:
            assert self.client_address[1]!=port
            record('request-held')
            control.sendall(b'Q'+self.client_address[1].to_bytes(2,'big'))
            assert control.recv(1)==b'R'
            ready=True
        self.send_response(200);self.end_headers();self.wfile.write(b'<html>fixture</html>')
        record('response-served')
    def log_message(self,*args):pass
server=http.server.HTTPServer(('127.0.0.1',port),Handler)
record('bound')
control.sendall(b'B'+port.to_bytes(2,'big'))
record('awaiting-request')
server.serve_forever()
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
        #[cfg(target_os = "linux")]
        std::env::set_var(
            "TYDE_TEST_HOST_NAMESPACE",
            std::fs::read_link("/proc/self/ns/mnt").expect("host mount namespace"),
        );
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
            DevScenario::NativeExit,
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
    #[cfg(target_os = "linux")]
    for scenario in [
        DevScenario::BubblewrapExit,
        DevScenario::RemoveDuringFrontend,
        DevScenario::RemoveDuringDesktop,
    ] {
        if desktop_marker.exists() {
            std::fs::remove_file(&desktop_marker).expect("reset completed desktop marker");
        }
        std::fs::remove_file(&control_path).expect("remove completed readiness socket");
        if matches!(scenario, DevScenario::RemoveDuringDesktop) {
            std::fs::write(desktop_marker.with_extension("hold"), "held")
                .expect("hold desktop readiness at real process boundary");
        }
        let error = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Bubblewrap test runtime")
            .block_on(start_dev_instance_expecting_exit(
                &control_path,
                &desktop_marker,
                scenario,
            ));
        if matches!(scenario, DevScenario::BubblewrapExit) {
            assert!(
                error.contains("exited before ready"),
                "DEV must reach the real child exit, not fail host chdir"
            );
        } else {
            assert!(
                error.contains("DEV startup cancelled"),
                "pending startup must observe explicit cancellation"
            );
        }
        if !matches!(scenario, DevScenario::RemoveDuringFrontend) {
            let proof: serde_json::Value = serde_json::from_slice(
                &std::fs::read(desktop_marker.with_extension("namespace"))
                    .expect("actual desktop process proof"),
            )
            .expect("desktop namespace proof");
            assert_eq!(
                proof,
                json!({"logical_cwd": true, "private_namespace": true})
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DevScenario {
    NativeExit,
    #[cfg(target_os = "linux")]
    BubblewrapExit,
    #[cfg(target_os = "linux")]
    RemoveDuringFrontend,
    #[cfg(target_os = "linux")]
    RemoveDuringDesktop,
}

impl DevScenario {
    fn removes_pending(self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self, Self::RemoveDuringFrontend | Self::RemoveDuringDesktop)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn holds_frontend(self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self, Self::RemoveDuringFrontend)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

async fn start_dev_instance_expecting_exit(
    control_path: &Path,
    desktop_marker: &Path,
    scenario: DevScenario,
) -> String {
    eprintln!("DEV READINESS scenario={scenario:?} phase=starting");
    let producer_proof = desktop_marker.with_extension("frontend");
    if producer_proof.exists() {
        std::fs::remove_file(&producer_proof).expect("remove completed producer proof");
    }
    let control_listener =
        tokio::net::UnixListener::bind(control_path).expect("bind frontend readiness gate");
    // Let the server retain its ephemeral listener: reserving then releasing a
    // port raced other tests and failed with AddressAlreadyInUse.
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        debug_mcp_bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    })
    .await;
    let fixture_root = tempfile::tempdir().expect("disposable project container");
    let workbench = if !matches!(scenario, DevScenario::NativeExit) {
        assert!(
            matches!(
                fixture.bootstrap.workbench_support.bubblewrap,
                protocol::WorkbenchAvailability::Available
            ),
            "DEV isolation regression requires Linux Bubblewrap with unprivileged user/mount namespaces; capability={:?}",
            fixture.bootstrap.workbench_support.bubblewrap
        );
        let unavailable = fixture_root.path().join("unavailable");
        std::fs::create_dir(&unavailable).expect("unrelated DEV project root");
        fixture
            .client
            .project_create(protocol::ProjectCreatePayload {
                name: "Unavailable unrelated project".into(),
                roots: vec![protocol::ProjectRootPath(
                    unavailable.to_string_lossy().into_owned(),
                )],
            })
            .await
            .expect("register unrelated project");
        next_frame_matching_on(&mut fixture.client, "unrelated project registered", |env| {
            env.kind == protocol::FrameKind::ProjectNotify
                && matches!(env.parse_payload::<protocol::ProjectNotifyPayload>().unwrap(),
                    protocol::ProjectNotifyPayload::Upsert { project } if project.name == "Unavailable unrelated project")
        }).await;
        std::fs::remove_dir(&unavailable).expect("make unrelated registered root unavailable");
        let physical = fixture_root.path().join("physical");
        std::fs::create_dir(&physical).expect("physical DEV source parent");
        let alias = fixture_root.path().join("alias");
        std::os::unix::fs::symlink(&physical, &alias).expect("symlinked DEV source parent");
        let source = alias.join("source");
        std::fs::create_dir_all(source.join("frontend/tauri-shell"))
            .expect("DEV fixture config directory");
        std::fs::create_dir(source.join("tools")).expect("DEV fixture tools directory");
        std::fs::write(
            source.join("tools/trunk-command.mjs"),
            "// explicit DEV workload fixture\n",
        )
        .expect("frontend workload source");
        std::fs::write(
            source.join("frontend/tauri-shell/tauri.conf.json"),
            "{\"build\":{}}",
        )
        .expect("DEV fixture config");
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=QA",
                "-c",
                "user.email=qa@localhost",
                "commit",
                "-qm",
                "DEV workload fixture",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(&source)
                    .args(args)
                    .status()
                    .expect("fixture Git command")
                    .success()
            );
        }
        fixture
            .client
            .project_create(protocol::ProjectCreatePayload {
                name: "DEV workload".into(),
                roots: vec![protocol::ProjectRootPath(
                    source.to_string_lossy().into_owned(),
                )],
            })
            .await
            .expect("create parent through protocol");
        let created = next_frame_matching_on(&mut fixture.client, "DEV parent project", |e| {
            e.kind == protocol::FrameKind::ProjectNotify
        })
        .await;
        let protocol::ProjectNotifyPayload::Upsert { project: parent } =
            created.parse_payload().expect("parent project payload")
        else {
            panic!("parent upsert required")
        };
        let write = fixture
            .client
            .replace_setting(
                "/workbenches/engine",
                protocol::WorkbenchEngine::Bubblewrap,
                protocol::WorkbenchEngine::Native,
            )
            .await
            .expect("select Bubblewrap engine");
        let result = next_frame_matching_on(&mut fixture.client, "DEV engine setting", |e| {
            e.kind == protocol::FrameKind::SettingsWriteResult
                && e.parse_payload::<protocol::SettingsWriteResultPayload>()
                    .unwrap()
                    .write_id
                    == write
        })
        .await;
        assert!(
            result
                .parse_payload::<protocol::SettingsWriteResultPayload>()
                .unwrap()
                .applied
        );
        fixture
            .client
            .workbench_create(protocol::WorkbenchCreatePayload {
                parent_project_id: parent.id,
                branch: protocol::GitBranchName("qa-dev-runtime".into()),
                name: "DEV runtime".into(),
            })
            .await
            .expect("create real engine workbench");
        let created = next_frame_matching_on(&mut fixture.client, "DEV workbench", |e| {
            matches!(
                e.kind,
                protocol::FrameKind::ProjectNotify | protocol::FrameKind::CommandError
            )
        })
        .await;
        assert_eq!(
            created.kind,
            protocol::FrameKind::ProjectNotify,
            "DEV isolation regression requires a strict-CoW filesystem; creation must succeed, reason={:?}",
            (created.kind == protocol::FrameKind::CommandError).then(|| created
                .parse_payload::<protocol::CommandErrorPayload>()
                .unwrap()
                .workbench_reason)
        );
        let protocol::ProjectNotifyPayload::Upsert { project } =
            created.parse_payload().expect("workbench payload")
        else {
            panic!("workbench upsert required")
        };
        Some(project)
    } else {
        None
    };
    let repo_root = match &workbench {
        Some(project) => PathBuf::from(&project.root_paths()[0].0),
        None => {
            Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").expect("current package directory"))
                .parent()
                .expect("repository root")
                .to_path_buf()
        }
    };
    let transport = StreamableHttpClientTransport::from_uri(
        fixture.host_for_test().debug_mcp_url_for_test().await,
    );
    let service = ().serve(transport).await.expect("connect to debug MCP");
    let (release_probe, held_probe) = std::sync::mpsc::channel();
    let (probe_entered_tx, probe_entered) = tokio::sync::oneshot::channel();
    let probe_root =
        std::fs::canonicalize(&repo_root).expect("canonical DEV source for probe observer");
    eprintln!(
        "DEV READINESS scenario={scenario:?} source_canonicalized={}",
        probe_root != repo_root
    );
    let probe_gate =
        fixture.on_project_scan(probe_root, server::ScanPoint::DevFrontendProbe, move || {
            let _ = probe_entered_tx.send(());
            let _ = held_probe.recv();
        });
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
        result = &mut start => {
            let details = match result {
                Ok(result) => result.content.into_iter().filter_map(|content| match content.raw {
                    RawContent::Text(text) => Some(text.text.replace(CREDENTIAL_SENTINEL, "[redacted]")),
                    _ => None,
                }).collect::<Vec<_>>(),
                Err(error) => vec![error.to_string()],
            };
            let producer = match std::fs::read_to_string(&producer_proof) {
                Ok(value) => value,
                Err(error) => format!("producer_proof_unavailable={:?}", error.kind()),
            };
            panic!("DEV READINESS scenario={scenario:?} settled before frontend gate: producer={producer} details={details:?}");
        },
        connected = control_listener.accept() => connected.expect("frontend entered actual readiness gate"),
    };
    assert_eq!(
        control.read_u8().await.expect("frontend control gate"),
        b'B'
    );
    assert!(
        !desktop_marker.exists(),
        "desktop started before frontend was ready"
    );
    let port = control.read_u16().await.expect("owned frontend endpoint");
    probe_entered
        .await
        .expect("real canonical probe is held before connecting");
    eprintln!("DEV READINESS scenario={scenario:?} phase=canonical-probe-held producer_bound=true");
    // The old producer announced B with no listener, letting a competing
    // endpoint (or self-connect) answer before the intended readiness gate.
    // B now attests a bound child; release only the real probe, not HTTP200.
    let ownership = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
    assert!(
        matches!(ownership, Err(error) if error.kind() == std::io::ErrorKind::AddrInUse),
        "frontend gate must retain its actual bound endpoint"
    );
    release_probe
        .send(())
        .expect("release actual canonical frontend probe");
    drop(probe_gate);
    assert_eq!(
        control
            .read_u8()
            .await
            .expect("owned frontend HTTP request"),
        b'Q'
    );
    let peer_port = control.read_u16().await.expect("actual probe endpoint");
    assert_ne!(peer_port, port, "readiness must not be a TCP self-connect");
    assert!(
        !desktop_marker.exists(),
        "desktop started before the held HTTP readiness response"
    );
    eprintln!(
        "DEV READINESS scenario={scenario:?} phase=owned-http-request local=127.0.0.1:{port} peer=127.0.0.1:{peer_port} self_connected=false awaiting_http200=true"
    );
    let mut desktop_control = None;
    if !scenario.holds_frontend() {
        control
            .write_all(b"R")
            .await
            .expect("release actual frontend readiness");
        if scenario.removes_pending() {
            let (mut connected, _) = tokio::select! {
                result = &mut start => panic!("dev start settled before desktop readiness gate: tool_completed={}", result.is_ok()),
                connected = control_listener.accept() => connected.expect("desktop entered actual readiness gate"),
            };
            assert_eq!(connected.read_u8().await.expect("desktop held event"), b'D');
            desktop_control = Some(connected);
        }
    }
    eprintln!("DEV READINESS scenario={scenario:?} phase=awaiting-terminal-result");
    let result = if scenario.removes_pending() {
        let project = workbench.as_ref().expect("owned Bubblewrap workbench");
        fixture
            .client
            .workbench_remove(protocol::WorkbenchRemovePayload {
                id: project.id.clone(),
                force: true,
            })
            .await
            .expect("remove workbench while actual startup readiness is held");
        let (start_result, removed) = tokio::join!(
            start.as_mut(),
            next_frame_matching_on(&mut fixture.client, "pending DEV workbench removed", |e| {
                e.kind == protocol::FrameKind::CommandError
                    || (e.kind == protocol::FrameKind::ProjectNotify
                        && matches!(e.parse_payload::<protocol::ProjectNotifyPayload>().unwrap(), protocol::ProjectNotifyPayload::Delete { project: p } if p.id == project.id))
            })
        );
        assert_eq!(
            removed.kind,
            protocol::FrameKind::ProjectNotify,
            "removal must join pending DEV processes without releasing readiness"
        );
        assert!(
            !repo_root.exists(),
            "joined workbench removal must remove the host root"
        );
        start_result.expect("cancelled DEV tool completes")
    } else {
        start.as_mut().await.expect("call tyde_dev_instance_start")
    };
    drop(start);
    let startup_error = result
        .content
        .iter()
        .filter_map(|content| match &content.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        desktop_marker.exists(),
        !scenario.holds_frontend(),
        "desktop launch must follow actual frontend readiness: tool_error={} frontend_exit={} frontend_connection={} frontend_response={} frontend_index={} port_in_use={} desktop_spawn={} cli_missing={}",
        result.is_error == Some(true),
        startup_error.contains("dev frontend exited before ready"),
        startup_error.contains("frontend connection failed"),
        startup_error.contains("frontend readiness"),
        startup_error.contains("frontend initial build did not serve"),
        startup_error.contains("Address already in use"),
        startup_error.contains("failed to spawn Tyde dev instance"),
        startup_error.contains("cargo-tauri was not found"),
    );
    if let Some(mut desktop) = desktop_control {
        assert_eq!(
            desktop
                .read_u8()
                .await
                .expect_err("held desktop must close")
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
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
        "failed or cancelled actual DEV process must report a tool error"
    );
    let content = result.content.first().expect("start error content");
    let RawContent::Text(text) = &content.raw else {
        panic!("expected text start error, got {:?}", content.raw);
    };
    let error = text.text.clone();
    if let Some(project) = workbench
        && !scenario.removes_pending()
    {
        fixture
            .client
            .workbench_remove(protocol::WorkbenchRemovePayload {
                id: project.id.clone(),
                force: true,
            })
            .await
            .expect("remove DEV workbench after joined startup failure");
        next_frame_matching_on(&mut fixture.client, "DEV workbench removed", |e| e.kind==protocol::FrameKind::ProjectNotify && matches!(e.parse_payload::<protocol::ProjectNotifyPayload>().unwrap(),protocol::ProjectNotifyPayload::Delete { project: p } if p.id==project.id)).await;
        assert!(!repo_root.exists());
    }
    eprintln!("DEV READINESS scenario={scenario:?} phase=terminal-cleanup-joined");
    error
}
