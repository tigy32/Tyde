#![cfg(target_os = "linux")]

mod fixture;

use fixture::{Fixture, next_logical_frame_matching_on};
use protocol::{
    AgentErrorPayload, BackendKind, FrameKind, NewAgentPayload, SpawnAgentParams, SpawnAgentPayload,
};
use server::backend::mock::{MockGateHandle, MockScript, MockTurn};
use server::subprocess::test_support;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn held_native_launch_preserves_protocol_and_process_ownership() {
    let mut fixture = Fixture::new().await;
    let uds = fixture.serve_uds().await;
    fixture.client = uds.connect().await;
    let directory = tempfile::tempdir().expect("create launch fixture directory");
    let marker = directory.path().join("unexpected-exec");
    let (process, installation) = test_support::Fixture::new(
        "/usr/bin/python3",
        &[
            "-c",
            "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()",
            marker.to_str().expect("UTF-8 fixture path"),
        ],
        directory.path(),
    );
    let reservation = fixture
        .reserve_next_mock_launch(
            "Held native launch",
            MockScript::new().with_native_process(process),
        )
        .await;
    fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Held native launch".to_owned()),
            launch_context: Default::default(),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![directory.path().to_string_lossy().into_owned()],
                prompt: "Controlled OS launch".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await
        .expect("request held startup through real UDS");
    let observer = installation
        .ready()
        .await
        .expect("observe host-owned native launch");
    let creation = observer
        .hold_creation()
        .await
        .expect("hold actual kernel creation");
    assert!(
        creation.shared_vm && creation.vfork,
        "native launch copied the host address space"
    );

    eprintln!("LAUNCH REGRESSION phase=creation_held");
    let second_connection = uds.connect().await;
    eprintln!("LAUNCH REGRESSION phase=unrelated_uds_bootstrap");
    let held_agent: NewAgentPayload = next_logical_frame_matching_on(
        &mut fixture.client,
        "pending native agent visibility",
        |frame| frame.kind == FrameKind::NewAgent,
    )
    .await
    .parse_payload()
    .expect("parse pending native agent visibility");
    assert!(
        held_agent.name == "Held native launch",
        "wrong pending launch stream"
    );
    eprintln!("LAUNCH REGRESSION phase=held_agent_visible");
    let response_gate = MockGateHandle::new();
    let agent = fixture
        .spawn_scripted(
            "Unrelated control",
            MockScript::one(MockTurn::text_after_gate(
                "Control acknowledged",
                &response_gate,
            )),
        )
        .await;
    assert!(
        agent.new_agent.name == "Unrelated control",
        "wrong unrelated launch stream"
    );
    eprintln!("LAUNCH REGRESSION phase=unrelated_agent_ready");
    response_gate.wait_until_entered().await;
    eprintln!("LAUNCH REGRESSION phase=unrelated_gate_entered");
    response_gate.release_one();
    eprintln!("LAUNCH REGRESSION phase=unrelated_gate_released");
    let mut saw_busy = false;
    loop {
        let frame = next_logical_frame_matching_on(
            &mut fixture.client,
            "unrelated turn while native creation held",
            |frame| frame.stream == agent.stream,
        )
        .await;
        eprintln!("LAUNCH REGRESSION unrelated_frame_kind={:?}", frame.kind);
        if frame.kind == FrameKind::ChatEvent
            && let protocol::ChatEvent::TypingStatusChanged(busy) =
                frame.parse_payload().expect("parse unrelated chat event")
        {
            eprintln!("LAUNCH REGRESSION unrelated_typing={busy}");
            if busy {
                saw_busy = true;
            } else if saw_busy {
                break;
            }
        }
    }
    fixture
        .client
        .close_agent(&agent.stream)
        .await
        .expect("close unrelated agent");
    next_logical_frame_matching_on(
        &mut fixture.client,
        "unrelated closure during held native startup",
        |frame| {
            frame.kind == FrameKind::AgentClosed
                && frame
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .is_ok_and(|closed| closed.agent_id == agent.new_agent.agent_id)
        },
    )
    .await;
    eprintln!("LAUNCH REGRESSION phase=unrelated_agent_closed");
    fixture
        .client
        .close_agent(&held_agent.instance_stream)
        .await
        .expect("close pending native launch through protocol");
    next_logical_frame_matching_on(
        &mut fixture.client,
        "pending native launch closes before native return",
        |frame| {
            frame.kind == FrameKind::AgentClosed
                && frame
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .is_ok_and(|closed| closed.agent_id == held_agent.agent_id)
        },
    )
    .await;
    assert!(
        !marker.exists(),
        "backend executed before releasing held native creation"
    );

    eprintln!("LAUNCH REGRESSION phase=startup_closed");
    observer
        .release(creation)
        .expect("release cancelled native creation");
    let completion = observer
        .until_reaped()
        .await
        .expect("observe late native cleanup");
    assert!(
        completion.native_returned && completion.cancelled,
        "host close did not retain ownership through late native return"
    );
    assert_eq!(
        completion.child_pids.len(),
        1,
        "late child did not acquire cleanup ownership"
    );
    for pid in completion.child_pids {
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "late native child was not reaped"
        );
    }
    assert!(
        !marker.exists(),
        "cancelled startup executed after its canonical close"
    );
    eprintln!("LAUNCH REGRESSION phase=cancelled_child_reaped");
    let (third_connection, bootstrap) = uds.connect_with_bootstrap().await;
    assert!(
        bootstrap.agents.is_empty(),
        "closed pending launch reappeared in host bootstrap"
    );
    assert_eq!(
        bootstrap.sessions.len(),
        1,
        "cancelled native startup published a late session"
    );

    eprintln!("LAUNCH REGRESSION phase=closed_state_verified");
    let sentinel = std::fs::File::open("/dev/null").expect("open host fd sentinel");
    let sentinel = unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_DUPFD, 100) };
    assert!(sentinel >= 100, "duplicate host fd sentinel");
    let sentinel = unsafe { OwnedFd::from_raw_fd(sentinel) };
    let socket_dir = tempfile::Builder::new()
        .prefix("tyde-proc-")
        .tempdir_in("/tmp")
        .expect("create short fixture control directory");
    let socket_path = socket_dir.path().join("control");
    let control = tokio::net::UnixListener::bind(&socket_path)
        .expect("bind real process fixture control socket");
    let target = r#"
import errno,os,sys
assert os.getcwd()==sys.argv[2]
assert os.environ['TYDE_LAUNCH_ENV']=='retained'
assert os.getpgrp()==os.getpid()
for fd in [3,4,5,6,int(sys.argv[3])]:
    try: os.fstat(fd)
    except OSError as error: assert error.errno==errno.EBADF
    else: raise RuntimeError('host descriptor inherited')
# libffi legitimately opens a new descriptor: inspect inheritance first.
import ctypes,socket,subprocess
signal=ctypes.c_int()
assert ctypes.CDLL(None).prctl(2,ctypes.byref(signal),0,0,0)==0 and signal.value==9
child=subprocess.Popen([sys.executable,'-c','import signal,sys; sys.stdout.write("R"); sys.stdout.flush(); signal.pause()'],stdout=subprocess.PIPE)
assert child.stdout.read(1)==b'R'
control=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
control.connect(sys.argv[1])
control.sendall(child.pid.to_bytes(4,'big'))
assert control.recv(1)==b'E'
os._exit(0)
"#;
    let (process, installation) = test_support::Fixture::new(
        "/usr/bin/python3",
        &[
            "-c",
            target,
            socket_path.to_str().expect("UTF-8 control path"),
            directory.path().to_str().expect("UTF-8 cwd"),
            &sentinel.as_raw_fd().to_string(),
        ],
        directory.path(),
    );
    let process = process.env("TYDE_LAUNCH_ENV", "retained");
    let reservation_healthy = fixture
        .reserve_next_mock_launch(
            "Healthy native launch",
            MockScript::one(MockTurn::text("Process contract complete"))
                .with_native_process(process),
        )
        .await;
    let request = SpawnAgentPayload {
        name: Some("Healthy native launch".to_owned()),
        launch_context: Default::default(),
        custom_agent_id: None,
        parent_agent_id: None,
        project_id: None,
        params: SpawnAgentParams::New {
            workspace_roots: vec![directory.path().to_string_lossy().into_owned()],
            prompt: "Real process contract".to_owned(),
            images: None,
            backend_kind: BackendKind::Claude,
            launch_profile_id: None,
            cost_hint: None,
            access_mode: Default::default(),
            session_settings: None,
        },
    };
    fixture
        .client
        .spawn_agent(request.clone())
        .await
        .expect("request healthy native launch");
    let observer = installation
        .ready()
        .await
        .expect("observe healthy host-owned native launch");
    let creation = observer
        .hold_creation()
        .await
        .expect("observe healthy native creation");
    assert!(
        creation.shared_vm && creation.vfork,
        "healthy creation left the canonical efficient path"
    );
    observer
        .release(creation)
        .expect("release healthy native creation");
    eprintln!("LAUNCH REGRESSION phase=healthy_creation_released");
    let reaper = tokio::spawn(observer.until_reaped());
    let (mut control, _) = control
        .accept()
        .await
        .expect("receive live real process contract");
    eprintln!("LAUNCH REGRESSION phase=healthy_target_connected");
    let descendant = control
        .read_u32()
        .await
        .expect("receive owned descendant handle");
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, descendant, 0) };
    assert!(pidfd >= 0, "open live descendant lifecycle descriptor");
    let pidfd = tokio::io::unix::AsyncFd::new(unsafe { OwnedFd::from_raw_fd(pidfd as i32) })
        .expect("observe live descendant lifecycle");
    eprintln!("LAUNCH REGRESSION phase=descendant_observed");
    let fourth_connection = uds.connect().await;
    control
        .write_all(b"E")
        .await
        .expect("release real leader exit");
    next_logical_frame_matching_on(
        &mut fixture.client,
        "successful process-backed startup",
        |frame| frame.kind == FrameKind::AgentStart,
    )
    .await;
    eprintln!("LAUNCH REGRESSION phase=healthy_start_published");
    let _dead = pidfd
        .readable()
        .await
        .expect("descendant survived leader-exit group cleanup");
    reaper
        .await
        .expect("healthy observer task failed")
        .expect("healthy child not reaped");

    eprintln!("LAUNCH REGRESSION phase=tree_cleanup_verified");
    let (process, installation) = test_support::Fixture::new(
        "/tyde-deliberately-missing-executable",
        &[],
        directory.path(),
    );
    let reservation_error = fixture
        .reserve_next_mock_launch(
            "Failed native launch",
            MockScript::new().with_native_process(process),
        )
        .await;
    let mut request = request;
    request.name = Some("Failed native launch".to_owned());
    fixture
        .client
        .spawn_agent(request)
        .await
        .expect("request failed native launch");
    let observer = installation
        .ready()
        .await
        .expect("observe failed native launch");
    let creation = observer
        .hold_creation()
        .await
        .expect("observe error-path creation");
    observer
        .release(creation)
        .expect("release error-path creation");
    let reaper = tokio::spawn(observer.until_reaped());
    next_logical_frame_matching_on(
        &mut fixture.client,
        "native exec failure reaches the protocol",
        |frame| {
            if frame.kind == FrameKind::AgentError {
                let error: AgentErrorPayload =
                    frame.parse_payload().expect("parse native startup error");
                return error.fatal && error.message.contains("os error 2");
            }
            false
        },
    )
    .await;
    reaper
        .await
        .expect("failed-exec observer task failed")
        .expect("failed exec child not reaped");
    eprintln!("LAUNCH REGRESSION phase=exec_error_published");
    let helper = std::env::var("TYDE_TEST_PROCESS_EXECUTABLE")
        .expect("canonical helper configured by repository check");
    let (process, installation) = test_support::Fixture::new(
        env!("TYDE_PARENT_DEATH_FIXTURE"),
        &[&helper],
        directory.path(),
    );
    let reservation_parent = fixture
        .reserve_next_mock_launch(
            "Parent lifetime contract",
            MockScript::one(MockTurn::text("Parent contract complete"))
                .with_native_process(process),
        )
        .await;
    let parent_agent = fixture
        .client
        .spawn_agent(SpawnAgentPayload {
            name: Some("Parent lifetime contract".to_owned()),
            launch_context: Default::default(),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: None,
            params: SpawnAgentParams::New {
                workspace_roots: vec![directory.path().to_string_lossy().into_owned()],
                prompt: "Parent lifetime contract".to_owned(),
                images: None,
                backend_kind: BackendKind::Claude,
                launch_profile_id: None,
                cost_hint: None,
                access_mode: Default::default(),
                session_settings: None,
            },
        })
        .await;
    parent_agent.expect("request real parent-lifetime fixture over UDS");
    let observer = installation
        .ready()
        .await
        .expect("observe parent-lifetime fixture");
    let creation = observer
        .hold_creation()
        .await
        .expect("observe parent fixture creation");
    assert!(
        creation.shared_vm && creation.vfork,
        "parent fixture bypassed efficient creation"
    );
    observer
        .release(creation)
        .expect("release parent fixture creation");
    let reaper = tokio::spawn(observer.until_reaped());
    let parent_ready = next_logical_frame_matching_on(
        &mut fixture.client,
        "parent-lifetime fixture completes before Backend admission",
        |frame| {
            assert!(
                frame.kind != FrameKind::AgentError,
                "parent-lifetime contract failed"
            );
            frame.kind == FrameKind::AgentStart
                && frame
                    .parse_payload::<protocol::AgentStartPayload>()
                    .is_ok_and(|start| start.name == "Parent lifetime contract")
        },
    )
    .await;
    assert_eq!(parent_ready.kind, FrameKind::AgentStart);
    reaper
        .await
        .expect("parent observer task failed")
        .expect("parent fixture not reaped");
    eprintln!("LAUNCH REGRESSION phase=parent_lifetime_races_verified");
    drop((
        reservation_parent,
        reservation,
        reservation_healthy,
        reservation_error,
        second_connection,
        third_connection,
        fourth_connection,
    ));
    uds.shutdown().await;
}
