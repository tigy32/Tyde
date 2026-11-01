//! Drives the production managed-remote lifecycle scripts (the ones the
//! desktop shell runs over SSH) against the real `tyde-server` binary in a
//! disposable `$HOME`.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};

use host_config::managed_host;
use host_config::{
    RemoteArchitecture, RemoteHostLifecycleSnapshot, RemoteOperatingSystem, RemotePlatform,
    RemoteTydeRunningState, TydeReleaseVersion,
};
use protocol::{
    Envelope, FrameKind, HelloPayload, PROTOCOL_VERSION, SeqValidator, StreamPath, TYDE_VERSION,
    WelcomePayload, read_envelope, write_envelope,
};

struct ManagedHome {
    dir: tempfile::TempDir,
    version: TydeReleaseVersion,
    untracked: std::cell::RefCell<Vec<Child>>,
}

impl ManagedHome {
    fn install() -> Self {
        let dir = tempfile::tempdir().expect("create disposable home");
        let version: TydeReleaseVersion = env!("CARGO_PKG_VERSION")
            .parse()
            .expect("crate version is a release version");
        let home = Self {
            dir,
            version,
            untracked: Default::default(),
        };
        let bin_dir = home.path().join(".tyde/bin").join(home.version.as_str());
        std::fs::create_dir_all(&bin_dir).expect("create managed bin dir");
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_tyde-server"),
            bin_dir.join("tyde-server"),
        )
        .expect("install managed tyde-server");
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn managed_bin(&self) -> PathBuf {
        self.path()
            .join(".tyde/bin")
            .join(self.version.as_str())
            .join("tyde-server")
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env("HOME", self.path())
            .env_remove("TYDE_SOCKET_PATH")
            .stdin(Stdio::null());
        command
    }

    fn sh(&self, script: &str) -> Output {
        self.command("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run lifecycle script")
    }

    fn probe(&self) -> RemoteHostLifecycleSnapshot {
        let output = self.sh(&managed_host::probe_script(&self.version));
        assert!(
            output.status.success(),
            "probe script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        managed_host::parse_probe_output(
            &String::from_utf8(output.stdout).expect("probe output is UTF-8"),
            RemotePlatform {
                os: RemoteOperatingSystem::Linux,
                arch: RemoteArchitecture::X86_64,
            },
            self.version.clone(),
        )
        .expect("parse probe output")
    }

    fn launch(&self) -> Output {
        self.sh(&managed_host::launch_script(&self.version))
    }

    fn stop(&self) {
        let output = self.sh(&managed_host::stop_script());
        assert!(
            output.status.success(),
            "stop script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Keeps an old-style, unregistered host alive under explicit test custody.
    fn spawn_untracked_server(&self, bin: &Path) -> u32 {
        let ready_dir = tempfile::Builder::new()
            .prefix("tyde-lifecycle-ready-")
            .tempdir()
            .unwrap();
        let ready_path = ready_dir.path().join("ready");
        let listener = std::os::unix::net::UnixListener::bind(&ready_path).unwrap();
        let child = self
            .command(bin)
            .args(["host", "--uds"])
            .env("TYDE_HOST_STARTUP_SOCKET", &ready_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn owned untracked server");
        let pid = child.id();
        self.untracked.borrow_mut().push(child);
        let (mut ready, _) = listener.accept().expect("actual host readiness event");
        let mut marker = [0; 4];
        ready
            .read_exact(&mut marker)
            .expect("complete readiness acknowledgement");
        assert_eq!(&marker, b"TYHR");
        pid
    }

    fn socket_is_live(&self) -> bool {
        std::os::unix::net::UnixStream::connect(self.path().join(".tyde/tyde.sock")).is_ok()
    }

    fn hold_legacy_launch_guard(&self) -> u32 {
        let mut child = self
            .command("/bin/sh")
            .args([
                "-c",
                r#"set -eu
mkdir -p "$HOME/.tyde/run"
launch_lock="$HOME/.tyde/run/tyde-host-launch.pid"
(set -C; printf '%s\n' "$$" > "$launch_lock")
trap 'rm -f "$launch_lock"' EXIT
printf 'LEGACY_OWNER_READY\n'
read -r completion || exit 0
"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start historical O_EXCL-only launch owner");
        let stdout = child.stdout.take().unwrap();
        let pid = child.id();
        self.untracked.borrow_mut().push(child);
        let mut ready = String::new();
        BufReader::new(stdout).read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "LEGACY_OWNER_READY");
        pid
    }

    fn release_legacy_launch_guard(&self, pid: u32) {
        let mut children = self.untracked.borrow_mut();
        let child = children.iter_mut().find(|child| child.id() == pid).unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }

    fn managed_server_pids(&self) -> Vec<u32> {
        pids_matching(&format!(
            "^{}/[^/]+/tyde-server host --uds( --managed)?$",
            self.path().join(".tyde/bin").display()
        ))
    }

    fn recorded_pid(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.path().join(".tyde/run/tyde-host.pid")).ok()?;
        Some(raw.trim().parse().expect("pid file holds a pid"))
    }

    fn launch_log(&self) -> String {
        let log = self
            .path()
            .join(".tyde/logs")
            .join(format!("tyde-host-{}.log", self.version.as_str()));
        std::fs::read_to_string(log).unwrap_or_default()
    }

    /// Launch completion must leave exactly one live socket owner.
    fn sole_managed_server(&self) -> u32 {
        let owners = self.managed_server_pids();
        assert_eq!(
            owners.len(),
            1,
            "completed launch must leave one live socket owner"
        );
        owners[0]
    }

    fn assert_managed_by(&self, owner: u32, context: &str) {
        assert_eq!(
            self.recorded_pid(),
            Some(owner),
            "{context}: the pid file must name the process that owns the socket"
        );
        assert_eq!(
            self.probe().running,
            RemoteTydeRunningState::Managed {
                version: self.version.clone()
            },
            "{context}: a reconnect must find the live managed server"
        );
        assert!(self.socket_is_live(), "{context}: socket must stay live");
    }
}

impl Drop for ManagedHome {
    fn drop(&mut self) {
        for pid in pids_matching(&format!("^{}/", self.path().display())) {
            kill(pid);
        }
        for child in self.untracked.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn pids_matching(pattern: &str) -> Vec<u32> {
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().parse().expect("pgrep prints pids"))
        .filter(|pid: &u32| {
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("read exact process state");
            let state = String::from_utf8_lossy(&state.stdout);
            !state.trim().is_empty() && !state.trim().starts_with('Z')
        })
        .collect()
}

fn kill(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id();
    child.wait().expect("reap true");
    pid
}

struct LaunchCaller {
    child: Child,
    stderr: BufReader<ChildStderr>,
    joined: bool,
}

impl LaunchCaller {
    fn waiting(home: &ManagedHome) -> Self {
        let shim = home.path().join("launch-no-delay");
        std::fs::create_dir_all(&shim).unwrap();
        let sleep = shim.join("sleep");
        std::fs::write(
            &sleep,
            "#!/bin/sh\nprintf 'launch attempted delay polling\\n' >&2\nexit 97\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sleep, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut path = vec![shim];
        path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut child = home
            .command("/bin/sh")
            .args(["-c", &managed_host::launch_script(&home.version)])
            .env("PATH", std::env::join_paths(path).unwrap())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("start owned launch caller");
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let mut caller = Self {
            child,
            stderr,
            joined: false,
        };
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                caller.stderr.read_line(&mut line).unwrap(),
                0,
                "Launch must enter its native descriptor wait, not fail or poll a delay while the resource owner is active"
            );
            if line.trim() == "tyde-server: waiting for managed launch owner" {
                break;
            }
        }
        caller
    }

    fn finish(mut self) {
        let status = self.child.wait().expect("join launch completion");
        self.joined = true;
        let mut stderr = String::new();
        self.stderr.read_to_string(&mut stderr).unwrap();
        assert!(status.success(), "event-released launch failed: {stderr}");
    }

    fn refuse_legacy_owner(mut self) {
        let status = self.child.wait().expect("join legacy guard refusal");
        self.joined = true;
        let mut stderr = String::new();
        self.stderr.read_to_string(&mut stderr).unwrap();
        assert!(!status.success());
        assert!(stderr.contains("legacy managed launch guard exists"));
    }

    fn cancel(mut self) {
        let status = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.child.id())])
            .status()
            .expect("cancel only owned caller process group");
        assert!(status.success());
        assert!(!self.child.wait().expect("join canceled caller").success());
        self.joined = true;
        let mut tail = Vec::new();
        self.stderr
            .read_to_end(&mut tail)
            .expect("all owned caller descriptors close after cancellation");
    }
}

impl Drop for LaunchCaller {
    fn drop(&mut self) {
        if !self.joined {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", self.child.id())])
                .status();
            let _ = self.child.wait();
        }
    }
}

struct LaunchLease(std::fs::File);
impl LaunchLease {
    fn hold(home: &ManagedHome) -> Self {
        let run = home.path().join(".tyde/run");
        std::fs::create_dir_all(&run).unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(run.join("tyde-host-launch.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&file).unwrap();
        Self(file)
    }
}
impl Drop for LaunchLease {
    fn drop(&mut self) {
        fs2::FileExt::unlock(&self.0).unwrap();
    }
}

#[test]
fn managed_host_lifecycle_tracks_the_process_that_owns_the_socket() {
    let home = ManagedHome::install();
    let fresh = home.probe();
    assert!(fresh.installed_target);
    assert_eq!(fresh.running, RemoteTydeRunningState::NotRunning);

    let legacy_owner = home.hold_legacy_launch_guard();
    let incompatible = home.launch();
    assert!(!incompatible.status.success());
    assert!(
        String::from_utf8_lossy(&incompatible.stderr)
            .contains("legacy managed launch guard exists")
    );
    assert!(home.managed_server_pids().is_empty());
    assert!(home.recorded_pid().is_none());
    assert!(!home.socket_is_live());
    home.release_legacy_launch_guard(legacy_owner);
    let legacy_guard = home.path().join(".tyde/run/tyde-host-launch.pid");
    let stale = dead_pid().to_string();
    std::fs::write(&legacy_guard, &stale).unwrap();
    let inode = std::fs::symlink_metadata(&legacy_guard).unwrap().ino();
    let stale_refused = home.launch();
    assert!(!stale_refused.status.success());
    assert!(
        String::from_utf8_lossy(&stale_refused.stderr)
            .contains("legacy managed launch guard exists")
    );
    assert!(std::fs::read_to_string(&legacy_guard).unwrap() == stale);
    assert_eq!(
        std::fs::symlink_metadata(&legacy_guard).unwrap().ino(),
        inode
    );
    assert!(home.managed_server_pids().is_empty());

    let lease = LaunchLease::hold(&home);
    let queued = LaunchCaller::waiting(&home);
    // The historical client replaces its stale marker while the new caller
    // actually waits on its lease. The replacement owner must not be reclaimed.
    std::fs::remove_file(&legacy_guard).unwrap();
    let replacement = home.hold_legacy_launch_guard();
    drop(lease);
    queued.refuse_legacy_owner();
    assert!(std::fs::read_to_string(&legacy_guard).unwrap().trim() == replacement.to_string());
    assert!(home.managed_server_pids().is_empty());
    home.release_legacy_launch_guard(replacement);

    // Two clients (or a superseded and a fresh reconnect attempt) launch at
    // once. Exactly one server can own the socket, and the pid file must name
    // that one, or every later reconnect relaunches into a bound socket.
    let lease = LaunchLease::hold(&home);
    let first = LaunchCaller::waiting(&home);
    let canceled = LaunchCaller::waiting(&home);
    canceled.cancel();
    let second = LaunchCaller::waiting(&home);
    drop(lease);
    first.finish();
    second.finish();
    assert!(!home.path().join(".tyde/run/tyde-host-launch.pid").exists());
    let owner = home.sole_managed_server();
    home.assert_managed_by(owner, "after concurrent launches");

    // A reconnect that launches again must settle on the live server rather
    // than start a second one against its socket.
    let relaunch = home.launch();
    assert!(
        relaunch.status.success(),
        "relaunch against a live managed server failed: {}",
        String::from_utf8_lossy(&relaunch.stderr)
    );
    assert_eq!(home.sole_managed_server(), owner);
    home.assert_managed_by(owner, "after relaunch");
    assert!(
        !home.launch_log().contains("UDS path is already in use"),
        "no launch may start a server against a bound socket:\n{}",
        home.launch_log()
    );

    let invalid_parent = home.path().join("not-a-directory");
    std::fs::write(&invalid_parent, b"owned error boundary").unwrap();
    let failed = home
        .command("/bin/sh")
        .args(["-c", &managed_host::launch_script(&home.version)])
        .env("TYDE_SOCKET_PATH", invalid_parent.join("host.sock"))
        .output()
        .expect("observe real launcher error");
    assert!(
        !failed.status.success(),
        "real startup errors must not be reported as readiness"
    );
    assert!(
        home.launch().status.success(),
        "failed caller must release its launch lease"
    );
    home.assert_managed_by(owner, "after failed caller");

    // SIGTERM runs production cleanup; disconnecting the earlier probes did
    // not shut down this shared host.
    home.stop();
    assert!(
        home.managed_server_pids().is_empty(),
        "completed stop must leave no live managed host"
    );
    assert_eq!(home.probe().running, RemoteTydeRunningState::NotRunning);

    // The state older releases left in the wild: a live managed server that
    // no pid file names, next to a pid file naming a dead process. The probe
    // must reattach to it, not report NotRunning and relaunch forever.
    let orphan = home.spawn_untracked_server(&home.managed_bin());
    let run_dir = home.path().join(".tyde/run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("tyde-host.pid"), format!("{}\n", dead_pid())).unwrap();
    std::fs::write(
        run_dir.join("tyde-host-version"),
        format!("{}\n", home.version.as_str()),
    )
    .unwrap();
    let pending_target: TydeReleaseVersion = "0.9.6-beta.999999".parse().unwrap();
    let pending_probe = home.sh(&managed_host::probe_script(&pending_target));
    assert!(
        pending_probe.status.success(),
        "an installation probe must not require the missing target's helper"
    );
    let pending = managed_host::parse_probe_output(
        &String::from_utf8(pending_probe.stdout).unwrap(),
        RemotePlatform {
            os: RemoteOperatingSystem::Linux,
            arch: RemoteArchitecture::X86_64,
        },
        pending_target,
    )
    .unwrap();
    assert!(!pending.installed_target);
    assert_eq!(
        pending.running,
        RemoteTydeRunningState::Managed {
            version: home.version.clone()
        }
    );
    assert_ne!(
        home.recorded_pid(),
        Some(orphan),
        "read-only installation probe must not publish an unleased adoption"
    );
    assert_eq!(
        home.probe().running,
        RemoteTydeRunningState::Managed {
            version: home.version.clone()
        },
        "an orphaned managed server must be adopted"
    );
    home.assert_managed_by(orphan, "after adopting an orphan");
    let launch = home.launch();
    assert!(
        launch.status.success(),
        "launch next to an adopted server failed: {}",
        String::from_utf8_lossy(&launch.stderr)
    );
    assert_eq!(home.sole_managed_server(), orphan);
    home.stop();
    assert!(
        !home.socket_is_live(),
        "completed stop must retract the adopted socket"
    );

    // A socket held by a server Tyde did not launch is never claimed.
    let manual_dir = home.path().join("manual");
    std::fs::create_dir_all(&manual_dir).unwrap();
    let manual_bin = manual_dir.join("tyde-server");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_tyde-server"), &manual_bin).unwrap();
    let manual = home.spawn_untracked_server(&manual_bin);
    assert_eq!(home.probe().running, RemoteTydeRunningState::UnknownSocket);
    let refused = home.launch();
    assert!(
        !refused.status.success(),
        "launch must refuse a socket owned by an unmanaged server"
    );
    assert_eq!(home.recorded_pid(), None);
    assert!(home.managed_server_pids().is_empty());
    kill(manual);
    let mut children = home.untracked.borrow_mut();
    let manual_child = children
        .iter_mut()
        .find(|child| child.id() == manual)
        .unwrap();
    manual_child.wait().expect("join manual server exit");
    drop(children);
    assert!(
        !home.socket_is_live(),
        "joined manual host must not accept connections"
    );

    // Once that server is gone its stale socket file must not block a launch.
    assert_eq!(home.probe().running, RemoteTydeRunningState::NotRunning);
    let launch = home.launch();
    assert!(
        launch.status.success(),
        "launch over a stale socket failed: {}",
        String::from_utf8_lossy(&launch.stderr)
    );
    let owner = home.sole_managed_server();
    home.assert_managed_by(owner, "after launching over a stale socket");
}

#[test]
fn managed_stop_refuses_a_reused_foreign_pid() {
    let home = ManagedHome::install();
    let mut foreign = home
        .command("cat")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn foreign process");
    let run = home.path().join(".tyde/run");
    std::fs::create_dir_all(&run).expect("create run directory");
    let pid_file = run.join("tyde-host.pid");
    std::fs::write(&pid_file, foreign.id().to_string()).expect("record stale pid binding");
    let result = home.sh(&managed_host::stop_script());
    let survived = foreign
        .try_wait()
        .expect("inspect foreign process")
        .is_none();
    drop(foreign.stdin.take());
    foreign.wait().expect("reap owned EOF-driven probe");
    assert!(
        !result.status.success(),
        "stop must reject a pid belonging to another command"
    );
    assert!(survived, "stop signalled an unrelated process");
    assert!(
        pid_file.exists(),
        "failed identity check must not remove ownership evidence"
    );
    std::fs::remove_file(pid_file).expect("remove probe pid file");
}

#[tokio::test]
async fn stdio_signals_exit_cleanly_with_the_client_still_connected() {
    for signal in ["-TERM", "-INT"] {
        let home = ManagedHome::install();
        eprintln!("Stdio signal check: signal={signal} phase=spawn");
        let log_path = home.path().join("stdio.log");
        let log = std::fs::File::create(&log_path).expect("stdio diagnostics");
        let mut child = tokio::process::Command::new(home.managed_bin())
            .args(["host", "--stdio"])
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn stdio host");
        let mut reader = tokio::io::BufReader::new(child.stdout.take().expect("host stdout"));
        let mut writer = child.stdin.take().expect("host stdin");
        let stream = StreamPath("/host/00000000-0000-4000-8000-000000000001".to_owned());
        let hello = Envelope::from_payload(
            stream.clone(),
            FrameKind::Hello,
            0,
            &HelloPayload {
                protocol_version: PROTOCOL_VERSION,
                tyde_version: TYDE_VERSION,
                client_name: "stdio-signal-test".to_owned(),
                platform: std::env::consts::OS.to_owned(),
            },
        )
        .expect("encode stdio hello");
        write_envelope(&mut writer, &hello)
            .await
            .expect("send stdio hello");
        eprintln!("Stdio signal check: signal={signal} phase=await-welcome");
        // The gate hit the client's ten-second startup timeout. This signal test
        // waits for protocol readiness and exit; the per-test watchdog bounds hangs.
        let response = read_envelope(&mut reader)
            .await
            .expect("read stdio handshake")
            .expect("host must respond before closing stdout");
        SeqValidator::new()
            .validate(&response.stream, response.seq, response.kind)
            .expect("valid stdio handshake sequence");
        assert!(response.stream == stream, "stdio handshake stream mismatch");
        assert_eq!(response.kind, FrameKind::Welcome);
        let welcome: WelcomePayload = response.parse_payload().expect("valid stdio welcome");
        assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
        assert_eq!(welcome.tyde_version, TYDE_VERSION);
        eprintln!("Stdio signal check: signal={signal} phase=send-signal");
        let sent = tokio::process::Command::new("kill")
            .args([signal, &child.id().expect("owned host pid").to_string()])
            .status()
            .await
            .expect("signal owned stdio host");
        assert!(sent.success());
        eprintln!("Stdio signal check: signal={signal} phase=await-exit-with-stdin-open");
        let exited = child.wait().await.expect("reap stdio host");
        let graceful_finished = std::fs::read_to_string(&log_path)
            .expect("read stdio diagnostics")
            .contains("stdio host graceful shutdown completed; leaving runtime");
        assert!(
            graceful_finished,
            "signal must complete graceful shutdown while client stdin remains open"
        );
        drop((reader, writer));
        assert!(
            exited.success(),
            "signal must finish production shutdown, not terminate the host abruptly"
        );
        eprintln!("Stdio signal check: signal={signal} phase=complete");
    }
}
