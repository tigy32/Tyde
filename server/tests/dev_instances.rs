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
config=json.load(open(path))
assert config['build']['beforeDevCommand'] is None
marker=pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER'])
marker.touch()
marker.with_suffix('.namespace').write_text(json.dumps({'logical_cwd':os.getcwd()==json.loads(marker.with_suffix('.logical').read_text()),'private_namespace':os.readlink('/proc/self/ns/mnt')!=os.environ['TYDE_TEST_HOST_NAMESPACE'] if pathlib.Path('/proc/self/ns/mnt').exists() else False}))
mode_path=marker.with_suffix('.mode')
def notify(signal,owned=False):
 address,port=os.environ['TYDE_DEV_INSTANCE_STARTUP_ADDR'].rsplit(':',1)
 with socket.create_connection((address,int(port))) as stream:
  stream.sendall(json.dumps({'protocol':json.load(open(pathlib.Path.cwd()/'devtools-protocol/dev-instance-startup.json'))['protocol'],'instance_id':os.environ['TYDE_DEV_INSTANCE_ID'],'signal':signal}).encode())
  stream.shutdown(socket.SHUT_WR)
  if owned:
   assert stream.recv(5,socket.MSG_WAITALL)==b'owned'
if mode_path.exists():
 application=marker.with_name(config['identifier'])
 notify({'kind':'application_directories','directories':[str(application)]},owned=True)
 (application/'webview').mkdir(parents=True)
 (application/'webview'/'state').write_text('owned')
if marker.with_suffix('.hold').exists():
 control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
 control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL']);control.sendall(b'D')
 if mode_path.exists():
  host_port=int(os.environ['TYDE_DEV_HOST_BIND_ADDR'].rsplit(':',1)[1])
  ui_port=int(os.environ['TYDE_DEV_UI_DEBUG_BIND_ADDR'].rsplit(':',1)[1])
  control.sendall(host_port.to_bytes(2,'big')+ui_port.to_bytes(2,'big'))
  host_port=int.from_bytes(control.recv(2,socket.MSG_WAITALL),'big')
  ui_port=int.from_bytes(control.recv(2,socket.MSG_WAITALL),'big')
 assert control.recv(1)==b'R'
 if mode_path.exists():
  bundle=config['identifier'] if sys.platform=='darwin' else None
  notify({'kind':'native_state','attestation':{'identity':{'applicationIdentifier':config['identifier'],'bundleIdentifier':bundle,'preferencesDomain':bundle},'webviews':[{'label':window['label'],'dataStore':'non_persistent' if window.get('incognito') else 'persistent'} for window in config['app']['windows']]}})
  notify({'kind':'host_listening','address':f'127.0.0.1:{host_port}'})
  if mode_path.read_text()=='ready':
   notify({'kind':'ui_listening','address':f'127.0.0.1:{ui_port}'})
   notify({'kind':'ui_ready'})
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

const FAKE_TRUNK: &str = r#"#!/usr/bin/python3
import json,os,pathlib,socket,sys
dist=pathlib.Path(sys.argv[sys.argv.index('--dist')+1])
marker=pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER'])
engine=json.loads(marker.with_suffix('.execution').read_text())
if engine=='bubblewrap':
 assert os.getcwd()==json.loads(marker.with_suffix('.logical').read_text()), 'frontend-logical-cwd'
 assert os.readlink('/proc/self/ns/mnt')!=os.environ['TYDE_TEST_HOST_NAMESPACE'], 'frontend-private-namespace'
elif engine=='native':
 if pathlib.Path('/proc/self/ns/mnt').exists():
  assert os.readlink('/proc/self/ns/mnt')==os.environ['TYDE_TEST_HOST_NAMESPACE'], 'frontend-native-namespace'
else:
 raise AssertionError('frontend-execution-kind')
marker.with_suffix('.frontend').write_text(json.dumps({'dist':str(dist),'wrapper_pid':os.getppid()}))
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL'])
control.sendall(b'B')
result=control.recv(1)
assert sys.argv[1]=='build', 'explicit-initial-build-result-required'
assert '--config' in sys.argv, 'exact-source-trunk-config-required'
assert dist.is_dir() and not list(dist.iterdir()), 'fresh-isolated-output-required'
if result==b'F':
 print('OWNED_INITIAL_BUILD_ERROR',file=sys.stderr,flush=True)
 sys.exit(17)
assert result==b'R', 'explicit-build-release-required'
(dist/'index.html').write_text('<html>owned frontend fixture</html>')
(dist/'fixture.js').write_text('owned asset')
# Compiler output must remain diagnostic, never a startup notification.
print('{"protocol":"owned_build_serve_v1","event":{"kind":"serving","port":1}}',flush=True)
"#;

const FOREIGN_FRONTEND_RECEIPT: &str = r#"#!/usr/bin/python3
import hashlib,json,os,pathlib,socket,sys
marker=pathlib.Path(os.environ['TYDE_TEST_DESKTOP_MARKER'])
dist=pathlib.Path(sys.argv[sys.argv.index('--dist')+1]);dist.mkdir()
marker.with_suffix('.frontend').write_text(json.dumps({'dist':str(dist),'wrapper_pid':os.getpid()}))
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(os.environ['TYDE_TEST_FRONTEND_CONTROL']);control.sendall(b'B')
assert control.recv(1)==b'R'
port=int.from_bytes(control.recv(2,socket.MSG_WAITALL),'big')
protocol=json.loads((pathlib.Path.cwd()/'devtools-protocol/dev-instance-startup.json').read_text())['frontend']
def notify(event):print(json.dumps({'protocol':protocol,'event':event}),flush=True)
print('Foreign serving receipt fixture',file=sys.stderr,flush=True)
notify({'kind':'build_finished','exit_code':0,'signal':None})
notify({'kind':'serving','port':port,'identity':'0'*64,'index_sha256':hashlib.sha256(b'foreign').hexdigest()})
assert control.recv(1)==b'X'
"#;

// Nextest isolates each test in its own process, so the process environment
// below belongs to this test alone. It is set before any runtime starts.
#[test]
fn dev_instance_claude_writes_stay_out_of_parent_claude_home() {
    let tools = tempfile::tempdir().expect("fake tool dir");
    let bin = tools.path().join("bin");
    std::fs::create_dir(&bin).expect("fake bin dir");
    executable(&bin.join("cargo-tauri"), FAKE_CARGO_TAURI);
    executable(&bin.join("trunk"), FAKE_TRUNK);
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
    std::fs::write(desktop_marker.with_extension("mode"), "exit")
        .expect("exit after creating announced native directories");

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
        DevScenario::NativeBuildFailure,
        DevScenario::NativeForeignIdentity,
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
        let node = bin.join("node");
        if matches!(scenario, DevScenario::NativeForeignIdentity) {
            executable(&node, FOREIGN_FRONTEND_RECEIPT);
        } else if node.exists() {
            std::fs::remove_file(node).expect("restore real Node wrapper");
        }
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
        } else if matches!(scenario, DevScenario::NativeForeignIdentity) {
            assert!(error.contains("Frontend serving identity mismatch"));
        } else if matches!(scenario, DevScenario::NativeBuildFailure) {
            assert!(error.contains("frontend initial build failed: exit_code=Some(17)"));
            assert!(error.contains("OWNED_INITIAL_BUILD_ERROR"));
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
    NativeBuildFailure,
    NativeForeignIdentity,
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
            Self::NativeExit
                | Self::NativeForeignIdentity
                | Self::NativeBuildFailure
                | Self::NativeProtocolMismatch
                | Self::NativeReady
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
    // A native scenario can itself run inside a Bubblewrap view at its
    // parent's path. Its source path does not establish whether the child
    // needs a private namespace.
    let engine = if scenario.is_native() {
        protocol::WorkbenchEngine::Native
    } else {
        protocol::WorkbenchEngine::Bubblewrap
    };
    std::fs::write(
        desktop_marker.with_extension("execution"),
        serde_json::to_vec(&engine).expect("typed expected execution kind"),
    )
    .expect("declare scenario namespace contract");
    let producer_proof = desktop_marker.with_extension("frontend");
    if producer_proof.exists() {
        std::fs::remove_file(&producer_proof).expect("remove completed producer proof");
    }
    let control_listener =
        tokio::net::UnixListener::bind(control_path).expect("bind frontend readiness gate");
    let mut fixture = Fixture::new_with_runtime_config(server::HostRuntimeConfig {
        debug_mcp_bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        ..Default::default()
    })
    .await;
    let fixture_root = tempfile::tempdir().expect("disposable project container");
    let (workbench, logical_root) = if !scenario.is_native() {
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
            include_str!("../../tools/trunk-command.mjs"),
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
            "DEV isolation regression requires working Bubblewrap namespaces; creation must succeed, reason={:?}",
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
        let logical_root = std::fs::canonicalize(&source)
            .expect("canonical DEV parent")
            .to_string_lossy()
            .into_owned();
        let protocol::ProjectSource::GitWorkbench {
            environment:
                protocol::WorkbenchEnvironment::Bubblewrap {
                    logical_root: view, ..
                },
            ..
        } = &project.source
        else {
            panic!("DEV workbench must be a Bubblewrap clone")
        };
        assert_eq!(
            view.0, logical_root,
            "the view must show the clone at its canonical parent path, not the symlinked alias"
        );
        (Some(project), Some(logical_root))
    } else {
        (None, None)
    };
    std::fs::write(
        desktop_marker.with_extension("logical"),
        serde_json::to_vec(&logical_root).expect("expected logical cwd"),
    )
    .expect("declare scenario logical cwd");
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
    probe_entered
        .await
        .expect("real canonical probe is held before connecting");
    eprintln!("DEV READINESS scenario={scenario:?} phase=canonical-probe-held build_held=true");
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

    release_probe
        .send(())
        .expect("release canonical startup pipe reader, not the build");
    drop(probe_gate);
    // A real unrelated HTTP listener can return either status while the owned
    // build remains held. Neither response is an owned build/serve receipt.
    let foreign = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("unrelated listener");
    let foreign_addr = foreign.local_addr().expect("unrelated endpoint");
    for status in [200, 404] {
        let request = async {
            let mut stream = tokio::net::TcpStream::connect(foreign_addr).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(
                String::from_utf8(response)
                    .unwrap()
                    .contains(&status.to_string())
            );
        };
        let respond = async {
            let (mut stream, _) = foreign.accept().await.unwrap();
            let mut request = [0; 256];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    format!("HTTP/1.1 {status} fixture\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
        };
        tokio::join!(request, respond);
        let pending =
            debug_json(&debug_call(service.peer(), "tyde_dev_instance_list", json!({})).await);
        assert_eq!(pending[0]["status"]["kind"], "starting");
        assert!(
            !desktop_marker.exists(),
            "foreign HTTP must not attest frontend readiness"
        );
    }
    let proof: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&producer_proof).expect("actual wrapper/build proof"),
    )
    .expect("producer proof JSON");
    let dist = PathBuf::from(proof["dist"].as_str().expect("isolated build output"));
    assert!(
        !dist.join("index.html").exists(),
        "held build has no ready index"
    );
    let wrapper_pid = proof["wrapper_pid"].as_i64().expect("owned wrapper pid") as libc::pid_t;
    let mut foreign = Some(foreign);
    let foreign_probe = if matches!(scenario, DevScenario::NativeForeignIdentity) {
        let foreign = foreign.take().expect("foreign listener fixture");
        Some(tokio::spawn(async move {
            let (mut stream, _) = foreign
                .accept()
                .await
                .expect("canonical probe reached foreign listener");
            let mut request = [0; 256];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nforeign")
                .await
                .unwrap();
            foreign
        }))
    } else {
        None
    };
    let mut desktop_control = None;
    let mut host_probe = None;
    let mut ui_probe = None;
    let mut competing_ui = None;
    if !scenario.holds_frontend() {
        control
            .write_all(if matches!(scenario, DevScenario::NativeBuildFailure) {
                b"F"
            } else {
                b"R"
            })
            .await
            .expect("release actual owned initial build result");
        if matches!(scenario, DevScenario::NativeForeignIdentity) {
            control
                .write_all(&foreign_addr.port().to_be_bytes())
                .await
                .unwrap();
        }
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
                let bound_host_port = host_listener.local_addr().unwrap().port();
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
                let mut bound_ui_port = 0;
                if !mismatch {
                    // Deterministically occupy the requested address before its
                    // owner binds. A released nonzero reservation fails here;
                    // port zero lets both owners keep distinct bound sockets.
                    competing_ui = Some(
                        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, ui_port))
                            .await
                            .expect("bind competing real UI-debug endpoint"),
                    );
                    let ui_listener = tokio::net::TcpListener::bind((
                        std::net::Ipv4Addr::LOCALHOST,
                        ui_port,
                    ))
                    .await
                    .expect("bind real UI-debug transport fixture while competitor remains bound");
                    let bound_ui_addr = ui_listener.local_addr().unwrap();
                    assert_ne!(
                        bound_ui_addr,
                        competing_ui.as_ref().unwrap().local_addr().unwrap(),
                        "live owners must receive distinct kernel-owned endpoints"
                    );
                    assert_eq!((host_port, ui_port), (0, 0));
                    bound_ui_port = bound_ui_addr.port();
                    eprintln!(
                        "DEV READINESS scenario={scenario:?} phase=bound-endpoint-ownership competitor_held=true distinct=true"
                    );
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
                    .write_u16(bound_host_port)
                    .await
                    .expect("announce bound host endpoint to desktop fixture");
                connected
                    .write_u16(bound_ui_port)
                    .await
                    .expect("announce bound UI-debug endpoint to desktop fixture");
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
    if let Some(probe) = foreign_probe {
        foreign = Some(probe.await.expect("join foreign HTTP responder"));
    }
    let foreign = foreign.expect("unrelated listener remains owned");
    let terminal = debug_json(&result);
    assert!(
        terminal["instanceId"].as_str() == Some(instance_id.as_str()),
        "terminal result must belong to the original attempt"
    );
    let owned = devtools_protocol::dev_instance_application_identifier(
        &devtools_protocol::DevInstanceId(instance_id.clone()),
    );
    let application_directory = desktop_marker.with_file_name(&owned);
    let startup_error = if matches!(scenario, DevScenario::NativeReady) {
        assert_ne!(result.is_error, Some(true));
        assert_eq!(terminal["status"]["kind"], "ready");
        assert_eq!(terminal["storesEphemeral"], true);
        let bundle = cfg!(target_os = "macos").then_some(owned.as_str());
        assert_eq!(
            terminal["nativeState"],
            json!({
                "identity": {
                    "applicationIdentifier": owned,
                    "bundleIdentifier": bundle,
                    "preferencesDomain": bundle,
                },
                "webviews": [{"label": "main", "dataStore": "non_persistent"}],
            }),
            "ready must attest instance-owned identity and non-persistent WebKit storage"
        );
        assert_eq!(
            terminal["applicationDirectories"],
            json!([application_directory]),
            "ready must report the announced instance-keyed native directories"
        );
        assert!(application_directory.join("webview/state").is_file());
        let store = terminal["storeDir"]
            .as_str()
            .expect("canonical ready store");
        assert!(Path::new(store).is_dir());
        assert_eq!(
            terminal["frontendBuild"]["protocol"],
            "owned_build_serve_v1"
        );
        use sha2::{Digest, Sha256};
        assert_eq!(
            terminal["frontendBuild"]["indexSha256"],
            format!(
                "{:x}",
                Sha256::digest(b"<html>owned frontend fixture</html>")
            )
        );
        let port = terminal["frontendPort"]
            .as_u64()
            .expect("owned serving port") as u16;
        assert_ne!(port, foreign_addr.port());
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("owned asset HTTP client");
        for (path, expected_status, expected_body) in [
            ("/", 200, "<html>owned frontend fixture</html>"),
            ("/fixture.js", 200, "owned asset"),
            ("/missing.js", 404, ""),
        ] {
            let response = http
                .get(format!("http://{address}{path}"))
                .send()
                .await
                .expect("owned serving endpoint");
            assert_eq!(response.status().as_u16(), expected_status);
            // Node's empty 404 uses a chunk terminator (0\r\n\r\n) on the
            // wire. Compare the decoded entity, not HTTP transfer framing.
            assert_eq!(
                response.text().await.expect("decoded HTTP entity"),
                expected_body
            );
        }
        #[cfg(target_os = "linux")]
        let listener_inode = std::fs::read_to_string("/proc/net/tcp")
            .unwrap()
            .lines()
            .skip(1)
            .find_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields[1] == format!("0100007F:{port:04X}") && fields[3] == "0A")
                    .then(|| fields[9].to_owned())
            })
            .expect("owned listener kernel identity");
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
        assert!(
            !application_directory.exists(),
            "explicit stop must remove the announced instance-keyed native directories"
        );
        #[cfg(target_os = "linux")]
        assert!(
            !std::fs::read_to_string("/proc/net/tcp")
                .unwrap()
                .lines()
                .skip(1)
                .any(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    fields[3] == "0A" && fields[9] == listener_inode
                }),
            "stop must release the exact owned serving socket"
        );
        String::new()
    } else {
        assert_eq!(
            result.is_error,
            Some(true),
            "actual startup failure must remain a tool error"
        );
        assert_eq!(terminal["status"]["kind"], "failed");
        if matches!(
            scenario,
            DevScenario::NativeExit | DevScenario::NativeProtocolMismatch
        ) {
            assert!(
                !application_directory.exists(),
                "startup failure must remove its announced native directories"
            );
        }
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
        !scenario.holds_frontend()
            && !matches!(
                scenario,
                DevScenario::NativeBuildFailure | DevScenario::NativeForeignIdentity
            ),
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
    assert!(
        !dist.exists(),
        "terminal cleanup must remove exact isolated build output"
    );
    assert_eq!(
        unsafe { libc::kill(wrapper_pid, 0) },
        -1,
        "joined cleanup must reap owned frontend wrapper"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    // Our unrelated services remain owned after instance cleanup.
    assert_eq!(foreign.local_addr().unwrap(), foreign_addr);
    if let Some(competitor) = competing_ui {
        let address = competitor
            .local_addr()
            .expect("competing socket still owned");
        let connect = tokio::net::TcpStream::connect(address);
        let accept = competitor.accept();
        let (connected, accepted) = tokio::join!(connect, accept);
        connected.expect("cleanup must not close an unrelated socket");
        accepted.expect("unrelated socket must still accept after DEV cleanup");
        eprintln!("DEV READINESS scenario={scenario:?} phase=competing-endpoint-preserved");
    }
    eprintln!("DEV READINESS scenario={scenario:?} phase=owned-cleanup-joined");
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
