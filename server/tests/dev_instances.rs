mod fixture;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fixture::{Fixture, next_frame_matching_on};
use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{Peer, RoleClient, ServiceExt};
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
 mode_path=marker.with_suffix('.mode')
 if mode_path.exists():
  host_port=int(os.environ['TYDE_DEV_HOST_BIND_ADDR'].rsplit(':',1)[1])
  ui_port=int(os.environ['TYDE_DEV_UI_DEBUG_BIND_ADDR'].rsplit(':',1)[1])
  control.sendall(host_port.to_bytes(2,'big')+ui_port.to_bytes(2,'big'))
 assert control.recv(1)==b'R'
 if mode_path.exists():
  def notify(signal):
   address,port=os.environ['TYDE_DEV_INSTANCE_STARTUP_ADDR'].rsplit(':',1)
   with socket.create_connection((address,int(port))) as stream:
    stream.sendall(json.dumps({'protocol':json.load(open(pathlib.Path.cwd()/'devtools-protocol/dev-instance-startup.json'))['protocol'],'instance_id':os.environ['TYDE_DEV_INSTANCE_ID'],'signal':signal}).encode())
    stream.shutdown(socket.SHUT_WR)
  notify({'kind':'host_listening','address':os.environ['TYDE_DEV_HOST_BIND_ADDR']})
  if mode_path.read_text()=='ready':notify({'kind':'ui_ready'})
  # No deadline: the owner stops this process after actual readiness/error.
  assert control.recv(1)==b'X'

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
proof=pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER']).with_suffix('.frontend')
def record(phase,port=None):
    details={'phase':phase,'pid':os.getpid()}
    if port is not None: details['port']=port
    proof.write_text(json.dumps(details))
    print(f'DEV FRONTEND phase={phase} port={port}',flush=True)
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL'])
record('control-connected')
record('validating-frontend-arguments')
assert sys.argv[1].endswith('/tools/trunk-command.mjs'), 'frontend-script-argument'
assert sys.argv[2]=='serve' and '--no-autoreload' in sys.argv, 'frontend-serve-arguments'
record('validating-workbench-namespace')
if sys.argv[1].startswith('/mnt/tyde-workspace/'):
 assert os.getcwd()=='/mnt/tyde-workspace', 'frontend-logical-cwd'
 assert os.readlink('/proc/self/ns/mnt')!=os.environ['TYDE_TEST_HOST_NAMESPACE'], 'frontend-private-namespace'
record('parsing-port')
port=int(sys.argv[sys.argv.index('--port')+1])
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
record('adopting-bound-listener',port)
listener=socket.socket(fileno=0)
assert listener.getsockname()==('127.0.0.1',port), 'inherited-frontend-endpoint'
server=http.server.HTTPServer(('127.0.0.1',port),Handler,bind_and_activate=False)
server.socket.close()
server.socket=listener
server.server_address=listener.getsockname()
server.server_name='localhost'
server.server_port=port
server.server_activate()
record('bound',port)
control.sendall(b'B'+port.to_bytes(2,'big'))
record('awaiting-request',port)
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
    let scenarios = [
        DevScenario::NativeProtocolMismatch,
        DevScenario::NativeReady,
        #[cfg(target_os = "linux")]
        DevScenario::BubblewrapExit,
        #[cfg(target_os = "linux")]
        DevScenario::RemoveDuringFrontend,
        #[cfg(target_os = "linux")]
        DevScenario::RemoveDuringDesktop,
    ];
    for scenario in scenarios {
        if desktop_marker.exists() {
            std::fs::remove_file(&desktop_marker).expect("reset completed desktop marker");
        }
        std::fs::remove_file(&control_path).expect("remove completed readiness socket");
        let hold = desktop_marker.with_extension("hold");
        if hold.exists() {
            std::fs::remove_file(&hold).expect("reset desktop gate");
        }
        let mode = desktop_marker.with_extension("mode");
        if mode.exists() {
            std::fs::remove_file(&mode).expect("reset native readiness mode");
        }
        if scenario.holds_desktop() {
            std::fs::write(&hold, "held").expect("hold desktop readiness at real process boundary");
        }
        match scenario {
            DevScenario::NativeProtocolMismatch => {
                std::fs::write(&mode, "mismatch").expect("select real handshake rejection")
            }
            DevScenario::NativeReady => {
                std::fs::write(&mode, "ready").expect("select typed readiness")
            }
            _ => {}
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
        if matches!(scenario, DevScenario::NativeReady) {
            assert!(
                error.is_empty(),
                "ready attempt must complete without startup error"
            );
        } else if matches!(scenario, DevScenario::NativeProtocolMismatch) {
            assert!(
                error.contains("IncompatibleProtocol"),
                "real protocol rejection must be a terminal startup error"
            );
        } else if !scenario.removes_pending() {
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
        if !scenario.is_native() && !scenario.holds_frontend() {
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
    NativeProtocolMismatch,
    NativeReady,
    #[cfg(target_os = "linux")]
    BubblewrapExit,
    #[cfg(target_os = "linux")]
    RemoveDuringFrontend,
    #[cfg(target_os = "linux")]
    RemoveDuringDesktop,
}

impl DevScenario {
    fn is_native(self) -> bool {
        matches!(
            self,
            Self::NativeExit | Self::NativeProtocolMismatch | Self::NativeReady
        )
    }

    fn holds_desktop(self) -> bool {
        matches!(self, Self::NativeProtocolMismatch | Self::NativeReady)
            || (self.removes_pending() && !self.holds_frontend())
    }

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
    // Keep both endpoints bound; transfer frontend ownership to the child
    // without the reserve-close-bind gap that caused AddressAlreadyInUse.
    let frontend_listener = std::sync::Arc::new(
        std::net::TcpListener::bind("127.0.0.1:0").expect("bind owned fixture frontend"),
    );
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        dev_frontend_listener: Some(frontend_listener),
        debug_mcp_bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    })
    .await;
    let fixture_root = tempfile::tempdir().expect("disposable project container");
    let workbench = if !scenario.is_native() {
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
        let contract = source.join(devtools_protocol::DEV_INSTANCE_STARTUP_CONTRACT_PATH);
        std::fs::create_dir_all(contract.parent().expect("contract parent"))
            .expect("DEV fixture contract directory");
        std::fs::write(
            contract,
            serde_json::to_vec(&devtools_protocol::DEV_INSTANCE_STARTUP_CONTRACT)
                .expect("typed child contract"),
        )
        .expect("declare supported fixture startup protocol");
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
    let debug_url = fixture.host_for_test().debug_mcp_url_for_test().await;
    let transport = StreamableHttpClientTransport::from_uri(debug_url.clone());
    let service = ().serve(transport).await.expect("connect to debug MCP");
    if matches!(scenario, DevScenario::NativeExit) {
        let unsupported = tempfile::tempdir().expect("unsupported source checkout");
        let contract = unsupported
            .path()
            .join(devtools_protocol::DEV_INSTANCE_STARTUP_CONTRACT_PATH);
        for declaration in [None, Some(r#"{"protocol":"unsupported"}"#)] {
            if let Some(declaration) = declaration {
                std::fs::create_dir_all(contract.parent().expect("contract parent"))
                    .expect("contract directory");
                std::fs::write(&contract, declaration).expect("unsupported child declaration");
            }
            let rejected = debug_call(
                service.peer(),
                "tyde_dev_instance_start",
                json!({"project_dir": unsupported.path().display().to_string()}),
            )
            .await;
            assert_eq!(
                rejected.is_error,
                Some(true),
                "unsupported source must fail before launching"
            );
            let content = rejected.content.first().expect("contract rejection");
            assert!(
                matches!(&content.raw, RawContent::Text(text) if text.text.contains("startup contract"))
            );
            let inventory =
                debug_json(&debug_call(service.peer(), "tyde_dev_instance_list", json!({})).await);
            assert!(
                inventory.as_array().expect("inventory").is_empty(),
                "rejected source must not create a startup attempt"
            );
            assert!(!desktop_marker.exists());
        }
    }
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
    let peer = service.peer().clone();
    let source = repo_root.display().to_string();
    let start = tokio::spawn(async move {
        debug_call(
            &peer,
            "tyde_dev_instance_start",
            json!({ "project_dir": source }),
        )
        .await
    });
    let (mut control, _) = control_listener
        .accept()
        .await
        .expect("frontend entered actual readiness gate");
    let marker = control.read_u8().await.expect("owned frontend bound event");
    assert_eq!(marker, b'B');
    assert!(
        !desktop_marker.exists(),
        "desktop started before frontend was ready"
    );
    let port = control.read_u16().await.expect("owned frontend endpoint");
    probe_entered
        .await
        .expect("real canonical probe is held before connecting");
    eprintln!("DEV READINESS scenario={scenario:?} phase=canonical-probe-held producer_bound=true");
    // Fails immediately on the old implementation while the real child is
    // held: its ready-only registry incorrectly reports no owned attempt.
    let listed = debug_json(&debug_call(service.peer(), "tyde_dev_instance_list", json!({})).await);
    let entries = listed.as_array().expect("DEV inventory array");
    assert_eq!(
        entries.len(),
        1,
        "pending startup must be registered before readiness"
    );
    let instance_id = entries[0]["instanceId"]
        .as_str()
        .expect("public pending handle")
        .to_owned();
    assert_eq!(entries[0]["status"]["kind"], "starting");
    assert!(
        entries[0].get("hostAddr").is_none(),
        "pending state must not attest host readiness"
    );
    let accepted = debug_json(&start.await.expect("immediate start acknowledgement"));
    assert!(
        accepted["instanceId"].as_str() == Some(instance_id.as_str()),
        "ack and inventory must identify one attempt"
    );
    // Discard the original connection/receipt. A new MCP client recovers the
    // same actor-owned attempt, rather than silently starting another child.
    service
        .cancel()
        .await
        .expect("disconnect original caller while startup is held");
    let service =
        ().serve(StreamableHttpClientTransport::from_uri(debug_url))
            .await
            .expect("reattach debug client");
    let snapshot = debug_json(
        &debug_call(
            service.peer(),
            "tyde_debug_snapshot",
            json!({"instance_id": instance_id}),
        )
        .await,
    );
    assert_eq!(snapshot["ready"], false);
    assert!(snapshot["instance"]["instanceId"].as_str() == Some(instance_id.as_str()));
    let duplicate = debug_call(
        service.peer(),
        "tyde_dev_instance_start",
        json!({"project_dir": repo_root.display().to_string()}),
    )
    .await;
    assert_eq!(
        duplicate.is_error,
        Some(true),
        "duplicate source start must be rejected"
    );
    let output = debug_json(
        &debug_call(
            service.peer(),
            "tyde_debug_events",
            json!({"instance_id": instance_id}),
        )
        .await,
    );
    assert!(
        output["nextCursor"].is_u64(),
        "pending output must expose a resumable cursor"
    );

    // The old producer announced B with no listener, letting a competing
    // endpoint (or self-connect) answer before the intended readiness gate.
    // B now attests a bound child; release only the real probe, not HTTP200.
    let ownership = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
    assert!(
        matches!(ownership, Err(error) if error.kind() == std::io::ErrorKind::AddrInUse),
        "frontend gate must retain its actual bound endpoint"
    );
    // Bind the cleanup probe while the child owns its port, so the later
    // release check cannot accidentally establish a TCP self-connection.
    let cleanup_probe = tokio::net::TcpSocket::new_v4().expect("create endpoint release probe");
    cleanup_probe
        .bind("127.0.0.1:0".parse().unwrap())
        .expect("bind distinct cleanup probe endpoint");
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
    let mut host_probe = None;
    let mut ui_probe = None;
    if !scenario.holds_frontend() {
        control
            .write_all(b"R")
            .await
            .expect("release actual frontend readiness");
        if scenario.holds_desktop() {
            let (mut connected, _) = control_listener
                .accept()
                .await
                .expect("desktop entered actual readiness gate");
            assert_eq!(connected.read_u8().await.expect("desktop held event"), b'D');
            if scenario.is_native() {
                let host_port = connected.read_u16().await.expect("owned host port");
                let ui_port = connected.read_u16().await.expect("owned UI-debug port");
                let host_listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, host_port))
                        .await
                        .expect("bind actual host protocol endpoint");
                let mismatch = matches!(scenario, DevScenario::NativeProtocolMismatch);
                host_probe = Some(tokio::spawn(async move {
                    let (stream, _) = host_listener
                        .accept()
                        .await
                        .expect("launcher connected to announced host");
                    let mut config = server::ServerConfig::current();
                    if mismatch {
                        config.protocol_version += 1;
                    }
                    match server::accept(&config, stream).await {
                        Ok(connection) => {
                            assert!(!mismatch, "incompatible real protocol must reject");
                            drop(connection);
                        }
                        Err(server::HandshakeError::IncompatibleProtocol { client, server }) => {
                            assert!(mismatch);
                            assert_eq!(client, protocol::PROTOCOL_VERSION);
                            assert_eq!(server, protocol::PROTOCOL_VERSION + 1);
                        }
                        Err(_) => panic!("unexpected host handshake failure"),
                    }
                }));
                if !mismatch {
                    let ui_listener =
                        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, ui_port))
                            .await
                            .expect("bind real UI-debug transport fixture");
                    ui_probe = Some(tokio::spawn(async move {
                        let (mut stream, _) = ui_listener
                            .accept()
                            .await
                            .expect("launcher UI readiness request");
                        let mut body = Vec::new();
                        stream
                            .read_to_end(&mut body)
                            .await
                            .expect("read typed UI request");
                        assert!(matches!(
                            serde_json::from_slice::<devtools_protocol::UiDebugRequest>(&body)
                                .expect("typed UI request"),
                            devtools_protocol::UiDebugRequest::Ping
                        ));
                        stream
                            .write_all(
                                &serde_json::to_vec(&devtools_protocol::UiDebugResponse::Pong)
                                    .expect("serialize typed Pong"),
                            )
                            .await
                            .expect("send UI readiness result");
                        stream.shutdown().await.expect("finish UI readiness result");
                    }));
                }
                connected
                    .write_all(b"R")
                    .await
                    .expect("release owned desktop startup notifications");
            }
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
            debug_call(
                service.peer(),
                "tyde_dev_instance_wait",
                json!({"instance_id": instance_id})
            ),
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
        start_result
    } else {
        debug_call(
            service.peer(),
            "tyde_dev_instance_wait",
            json!({"instance_id": instance_id}),
        )
        .await
    };
    if let Some(probe) = host_probe {
        probe.await.expect("join actual host handshake");
    }
    if let Some(probe) = ui_probe {
        probe.await.expect("join actual UI readiness transport");
    }
    let terminal = debug_json(&result);
    assert!(
        terminal["instanceId"].as_str() == Some(instance_id.as_str()),
        "terminal result must belong to the original attempt"
    );
    let startup_error = if matches!(scenario, DevScenario::NativeReady) {
        assert_ne!(result.is_error, Some(true));
        assert_eq!(terminal["status"]["kind"], "ready");
        assert_eq!(terminal["storesEphemeral"], true);
        let store = terminal["storeDir"]
            .as_str()
            .expect("canonical ready store");
        assert!(Path::new(store).is_dir());
        let stop = debug_call(
            service.peer(),
            "tyde_dev_instance_stop",
            json!({"instance_id": instance_id}),
        )
        .await;
        assert_ne!(stop.is_error, Some(true));
        assert_eq!(debug_json(&stop)["status"]["kind"], "stopped");
        assert!(
            !Path::new(store).exists(),
            "explicit stop must join private resource cleanup"
        );
        String::new()
    } else {
        assert_eq!(
            result.is_error,
            Some(true),
            "actual startup failure must remain a tool error"
        );
        assert_eq!(terminal["status"]["kind"], "failed");
        if matches!(scenario, DevScenario::NativeProtocolMismatch) {
            assert_eq!(terminal["status"]["phase"], "host_handshake");
            assert_eq!(
                terminal["status"]["failure"]["kind"],
                "host_protocol_incompatible"
            );
            assert_eq!(
                terminal["status"]["failure"]["client"],
                protocol::PROTOCOL_VERSION
            );
            assert_eq!(
                terminal["status"]["failure"]["server"],
                protocol::PROTOCOL_VERSION + 1
            );
        }
        let replay = debug_call(
            service.peer(),
            "tyde_dev_instance_wait",
            json!({"instance_id": instance_id}),
        )
        .await;
        assert_eq!(replay.is_error, Some(true));
        assert!(
            debug_json(&replay) == terminal,
            "reattach must preserve the same terminal failure"
        );
        let output = debug_json(
            &debug_call(
                service.peer(),
                "tyde_debug_events",
                json!({"instance_id": instance_id}),
            )
            .await,
        );
        let snapshot = debug_json(
            &debug_call(
                service.peer(),
                "tyde_debug_snapshot",
                json!({"instance_id": instance_id}),
            )
            .await,
        );
        assert!(
            output["nextCursor"]
                .as_u64()
                .expect("settled output cursor")
                > 1,
            "fixture must exercise a nontrivial output window"
        );
        assert_eq!(
            snapshot["outputCursor"], output["nextCursor"],
            "snapshot must identify the actual output head"
        );
        let resumed = debug_json(
            &debug_call(
                service.peer(),
                "tyde_debug_events",
                json!({"instance_id": instance_id, "cursor": snapshot["outputCursor"]}),
            )
            .await,
        );
        assert!(
            resumed["events"]
                .as_array()
                .expect("resumed output events")
                .is_empty(),
            "resuming a settled snapshot must not replay old output"
        );
        terminal["status"]["error"]
            .as_str()
            .expect("typed startup failure")
            .to_owned()
    };
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
    let released = cleanup_probe
        .connect((std::net::Ipv4Addr::LOCALHOST, port).into())
        .await
        .expect_err("joined startup cleanup must release the child's listener");
    assert_eq!(released.kind(), std::io::ErrorKind::ConnectionRefused);
    eprintln!("DEV READINESS scenario={scenario:?} phase=endpoint-released-after-cleanup");
    service.cancel().await.expect("cancel debug MCP client");
    let error = startup_error;
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

async fn debug_call(
    peer: &Peer<RoleClient>,
    name: &str,
    arguments: serde_json::Value,
) -> CallToolResult {
    peer.call_tool(CallToolRequestParams {
        meta: None,
        name: name.to_owned().into(),
        arguments: arguments.as_object().cloned(),
        task: None,
    })
    .await
    .expect("debug MCP boundary call")
}

fn debug_json(result: &CallToolResult) -> serde_json::Value {
    let content = result.content.first().expect("debug MCP JSON content");
    let RawContent::Text(text) = &content.raw else {
        panic!("debug MCP result must contain JSON text");
    };
    serde_json::from_str(&text.text).expect("typed debug MCP JSON result")
}
