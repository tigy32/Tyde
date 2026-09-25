//! Drives the production managed-remote lifecycle scripts (the ones the
//! desktop shell runs over SSH) against the real `tyde-server` binary in a
//! disposable `$HOME`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use host_config::managed_host;
use host_config::{
    RemoteArchitecture, RemoteHostLifecycleSnapshot, RemoteOperatingSystem, RemotePlatform,
    RemoteTydeRunningState, TydeReleaseVersion,
};

const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

struct ManagedHome {
    dir: tempfile::TempDir,
    version: TydeReleaseVersion,
}

impl ManagedHome {
    fn install() -> Self {
        let dir = tempfile::tempdir().expect("create disposable home");
        let version: TydeReleaseVersion = env!("CARGO_PKG_VERSION")
            .parse()
            .expect("crate version is a release version");
        let home = Self { dir, version };
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

    /// Starts `tyde-server host --uds` the way releases before the
    /// server-owned pid file did: detached, with nothing recording the pid.
    fn spawn_untracked_server(&self, bin: &Path) -> u32 {
        let output = self.sh(&format!(
            "nohup {} host --uds > /dev/null 2>&1 < /dev/null &\necho $!",
            managed_host::shell_quote(&bin.display().to_string())
        ));
        let pid = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("shell prints the server pid");
        wait_until("untracked server to accept connections", || {
            self.socket_is_live()
        });
        pid
    }

    fn socket_is_live(&self) -> bool {
        std::os::unix::net::UnixStream::connect(self.path().join(".tyde/tyde.sock")).is_ok()
    }

    fn managed_server_pids(&self) -> Vec<u32> {
        pids_matching(&format!("^{}/", self.path().join(".tyde/bin").display()))
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

    /// Waits for every server that lost the socket race to exit, and returns
    /// the one process left holding it.
    fn sole_managed_server(&self) -> u32 {
        wait_until("exactly one managed server to remain", || {
            self.managed_server_pids().len() == 1
        });
        self.managed_server_pids()[0]
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
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
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

#[test]
fn managed_host_lifecycle_tracks_the_process_that_owns_the_socket() {
    let home = ManagedHome::install();
    let fresh = home.probe();
    assert!(fresh.installed_target);
    assert_eq!(fresh.running, RemoteTydeRunningState::NotRunning);

    // Two clients (or a superseded and a fresh reconnect attempt) launch at
    // once. Exactly one server can own the socket, and the pid file must name
    // that one, or every later reconnect relaunches into a bound socket.
    let launches = std::thread::scope(|scope| {
        let first = scope.spawn(|| home.launch());
        let second = scope.spawn(|| home.launch());
        [first.join().unwrap(), second.join().unwrap()]
    });
    for launch in &launches {
        assert!(
            launch.status.success(),
            "concurrent launch failed: {}",
            String::from_utf8_lossy(&launch.stderr)
        );
    }
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

    // SIGTERM runs production cleanup; disconnecting the earlier probes did
    // not shut down this shared host.
    home.stop();
    wait_until("stopped server to exit", || {
        home.managed_server_pids().is_empty()
    });
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
    wait_until("adopted server to exit", || !home.socket_is_live());

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
    wait_until("unmanaged server to exit", || !home.socket_is_live());

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
        .command("sleep")
        .arg("120")
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
    let _ = foreign.kill();
    foreign.wait().expect("reap owned probe");
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
        let io = tokio::io::join(
            child.stdout.take().expect("host stdout"),
            child.stdin.take().expect("host stdin"),
        );
        let connection = tokio::time::timeout(
            Duration::from_secs(15),
            client::connect(&client::ClientConfig::current(), io),
        )
        .await
        .expect("stdio handshake deadline")
        .expect("real stdio handshake");
        let sent = tokio::process::Command::new("kill")
            .args([signal, &child.id().expect("owned host pid").to_string()])
            .status()
            .await
            .expect("signal owned stdio host");
        assert!(sent.success());
        let exited = tokio::time::timeout(Duration::from_secs(28), child.wait()).await;
        let graceful_finished = std::fs::read_to_string(&log_path)
            .expect("read stdio diagnostics")
            .contains("stdio host graceful shutdown completed; leaving runtime");
        assert!(
            exited.is_ok(),
            "signal shutdown waited for stdin EOF; graceful shutdown completed={graceful_finished}"
        );
        let exited = exited.expect("checked timeout").expect("reap stdio host");
        drop(connection);
        assert!(
            exited.success(),
            "signal must finish production shutdown, not terminate the host abruptly"
        );
    }
}
