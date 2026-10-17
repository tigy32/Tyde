//! Linux requires pidfds/procfs; Darwin uses native spawn and SIGCHLD readiness.
//! Native capability failures are errors;
//! this path never retries process creation through fork or pre-exec hooks.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::ExitStatus;

#[cfg(target_os = "linux")]
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{GroupCommand, ProcessStdio, SessionMode};

#[cfg(feature = "test-support")]
static EXECUTABLE_ENTRYPOINT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const HELPER_MODE: &str = "--tyde-process-exec";
const CONTROL: RawFd = 3;
#[cfg(target_os = "linux")]
const PARENT: RawFd = 4;
#[cfg(target_os = "linux")]
const CREATOR: RawFd = 6;
const EXECUTABLE: RawFd = 5;
const READY: &[u8; 4] = b"TYRD";
const ERROR: &[u8; 4] = b"TYER";

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
enum Stage {
    Parent = 1,
    Descriptors = 2,
    Plan = 3,
    Exec = 4,
    Session = 5,
}

fn syscall(value: libc::c_int) -> io::Result<libc::c_int> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn posix(value: libc::c_int) -> io::Result<()> {
    if value == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(value))
    }
}
fn cstring(value: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}
fn argument_limit() -> io::Result<usize> {
    let limit = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    usize::try_from(limit)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| io::Error::other("cannot determine Linux exec argument limit"))
}
fn high_fd(fd: OwnedFd) -> io::Result<OwnedFd> {
    let minimum = if cfg!(target_os = "linux") { 7 } else { 6 };
    let raw = syscall(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, minimum) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    #[cfg(target_os = "linux")]
    syscall(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    #[cfg(not(target_os = "linux"))]
    syscall(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((high_fd(read)?, high_fd(write)?))
}

struct Actions(libc::posix_spawn_file_actions_t);
impl Actions {
    fn new() -> io::Result<Self> {
        let mut value = MaybeUninit::uninit();
        posix(unsafe { libc::posix_spawn_file_actions_init(value.as_mut_ptr()) })?;
        Ok(Self(unsafe { value.assume_init() }))
    }
    fn duplicate(&mut self, from: RawFd, to: RawFd) -> io::Result<()> {
        posix(unsafe { libc::posix_spawn_file_actions_adddup2(&mut self.0, from, to) })
    }
}
impl Drop for Actions {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.0);
        }
    }
}
struct Attributes(libc::posix_spawnattr_t);
impl Attributes {
    fn new(session: SessionMode) -> io::Result<Self> {
        let mut value = MaybeUninit::uninit();
        posix(unsafe { libc::posix_spawnattr_init(value.as_mut_ptr()) })?;
        let mut value = Self(unsafe { value.assume_init() });
        let group = matches!(session, SessionMode::Group);
        if group {
            posix(unsafe { libc::posix_spawnattr_setpgroup(&mut value.0, 0) })?;
        }
        let mut signals = MaybeUninit::uninit();
        syscall(unsafe { libc::sigemptyset(signals.as_mut_ptr()) })?;
        let mut signals = unsafe { signals.assume_init() };
        syscall(unsafe { libc::sigaddset(&mut signals, libc::SIGPIPE) })?;
        if matches!(session, SessionMode::Terminal) {
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                syscall(unsafe { libc::sigaddset(&mut signals, signal) })?;
            }
        }
        posix(unsafe { libc::posix_spawnattr_setsigdefault(&mut value.0, &signals) })?;
        posix(unsafe {
            libc::posix_spawnattr_setflags(
                &mut value.0,
                (if group {
                    libc::POSIX_SPAWN_SETPGROUP
                } else {
                    0
                } | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short,
            )
        })?;
        Ok(value)
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawnattr_destroy(&mut self.0);
        }
    }
}

struct Pid {
    pid: libc::pid_t,
    reaped: bool,
    handed_off: bool,
}
impl Pid {
    fn kill_group(&self) -> io::Result<()> {
        if self.reaped || self.handed_off {
            return Ok(());
        }
        if let Err(error) = syscall(unsafe { libc::kill(self.pid, libc::SIGKILL) })
            && error.raw_os_error() != Some(libc::ESRCH)
        {
            return Err(error);
        }
        match syscall(unsafe { libc::kill(-self.pid, libc::SIGKILL) }) {
            Ok(_) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }
    fn reap(&mut self) -> io::Result<ExitStatus> {
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result == self.pid {
                self.reaped = true;
                return Ok(ExitStatus::from_raw(status));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
impl Drop for Pid {
    fn drop(&mut self) {
        if !self.reaped {
            if let Err(error) = self.kill_group() {
                tracing::error!(error_kind=?error.kind(),"owned Linux group cleanup failed");
            }
            if let Err(error) = self.reap() {
                tracing::error!(error_kind=?error.kind(),"owned Linux child reaping failed");
            }
        }
    }
}

pub(super) struct Readiness {
    #[cfg(target_os = "linux")]
    descriptor: AsyncFd<OwnedFd>,
    #[cfg(not(target_os = "linux"))]
    changed: tokio::sync::Notify,
}
impl Readiness {
    pub(super) async fn wait(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let _ready = self.descriptor.readable().await?;
        }
        #[cfg(not(target_os = "linux"))]
        self.changed.notified().await;
        Ok(())
    }
}
pub(super) struct Child {
    pid: Pid,
    ready: std::sync::Arc<Readiness>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    status: Option<ExitStatus>,
}
impl Child {
    pub(super) fn handoff(&mut self) -> io::Result<()> {
        self.pid.handed_off = true;
        Ok(())
    }
    pub(super) fn readiness(&self) -> std::sync::Arc<Readiness> {
        std::sync::Arc::clone(&self.ready)
    }
    pub(super) fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.stdin.take()
    }
    pub(super) fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.stdout.take()
    }
    pub(super) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.stderr.take()
    }
    pub(super) fn id(&self) -> Option<u32> {
        if self.pid.reaped {
            None
        } else {
            Some(self.pid.pid as u32)
        }
    }
    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        self.pid.kill_group()
    }
    pub(super) fn signal(&self, signal: command_group::Signal) -> io::Result<()> {
        if self.pid.reaped {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        syscall(unsafe { libc::kill(-self.pid.pid, signal as libc::c_int) }).map(|_| ())
    }
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
        syscall(unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid.pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        })?;
        let info = unsafe { info.assume_init() };
        #[cfg(target_os = "linux")]
        let observed = unsafe { info.si_pid() };
        #[cfg(not(target_os = "linux"))]
        let observed = info.si_pid;
        if observed == 0 {
            return Ok(None);
        }
        // Keep the leader unreaped until signalling: its PID/PGID cannot be
        // recycled to an unrelated process between exit observation and kill.
        self.pid.kill_group()?;
        let status = self.pid.reap()?;
        self.status = Some(status);
        Ok(Some(status))
    }
    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            self.ready.wait().await?;
        }
    }
}

pub(super) struct Exchange {
    socket: tokio::net::UnixStream,
    plan: Vec<u8>,
    independent: bool,
    spec: GroupCommand,
}
impl Exchange {
    pub(super) async fn complete(
        mut self,
        commit: tokio::sync::oneshot::Sender<tokio::sync::oneshot::Sender<()>>,
        launch: std::sync::Arc<super::PendingLaunch>,
        admission: Option<super::DeferredPlan>,
    ) -> io::Result<()> {
        let read = &mut self.socket;
        let response = async {
            let mut marker = [0; 4];
            read.read_exact(&mut marker).await.map_err(|error| {
                io::Error::new(
                    error.kind(),
                    "process executable did not establish the exec-helper protocol",
                )
            })?;
            if marker == *READY {
                launch.trace(super::LaunchPhase::HelperPrepared);
                if let Some(admission) = admission {
                    admission
                        .ready
                        .send(())
                        .map_err(|_| super::cancelled_launch_error())?;
                    self.spec.program = admission
                        .resume
                        .await
                        .map_err(|_| super::cancelled_launch_error())?;
                    self.plan = encode(&self.spec)?;
                }
                read.write_all(&self.plan).await?;
                if self.independent {
                    let mut prepared = [0; 4];
                    read.read_exact(&mut prepared).await?;
                    if prepared == *ERROR {
                        marker = prepared;
                    } else {
                        if prepared != *b"TYHO" {
                            return Err(io::Error::other("invalid helper handoff preparation"));
                        }
                        let (grant, accepted) = tokio::sync::oneshot::channel();
                        commit
                            .send(grant)
                            .map_err(|_| super::cancelled_launch_error())?;
                        tokio::select! {
                            biased;
                            _=launch.cancellation.cancelled()=>return Err(super::cancelled_launch_error()),
                            result=accepted=>result.map_err(|_|super::cancelled_launch_error())?,
                        }
                        read.write_all(b"C").await?;
                    }
                }
                if marker != *ERROR {
                    let mut byte = [0];
                    if read.read(&mut byte).await? == 0 {
                        return Ok(());
                    }
                    marker[0] = byte[0];
                    read.read_exact(&mut marker[1..]).await?;
                }
            }
            if marker != *ERROR {
                return Err(io::Error::other("invalid exec-helper status"));
            }
            let mut failure = [0; 5];
            read.read_exact(&mut failure).await?;
            let stage = match failure[0] {
                1 => Stage::Parent,
                2 => Stage::Descriptors,
                3 => Stage::Plan,
                4 => Stage::Exec,
                5 => Stage::Session,
                _ => return Err(io::Error::other("invalid exec-helper failure stage")),
            };
            let error = io::Error::from_raw_os_error(i32::from_be_bytes(
                failure[1..].try_into().map_err(io::Error::other)?,
            ));
            tracing::error!(?stage,error_kind=?error.kind(),"backend exec helper failed");
            Err(error)
        };
        response.await?;
        Ok(())
    }
}

fn encode(spec: &GroupCommand) -> io::Result<Vec<u8>> {
    fn field(buffer: &mut Vec<u8>, value: &std::ffi::OsStr) -> io::Result<()> {
        let value = cstring(value)?;
        buffer.extend_from_slice(
            &u32::try_from(value.as_bytes().len())
                .map_err(io::Error::other)?
                .to_be_bytes(),
        );
        buffer.extend_from_slice(value.as_bytes());
        Ok(())
    }
    let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
    #[cfg(feature = "test-support")]
    env.remove(std::ffi::OsStr::new("TYDE_TEST_PROCESS_EXECUTABLE"));
    for (key, value) in &spec.env {
        if key.as_bytes().contains(&b'=') {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        match value {
            Some(value) => {
                env.insert(key.clone(), value.clone());
            }
            None => {
                env.remove(key);
            }
        }
    }
    let mut closed_stdio = 0_u8;
    for (fd, mode) in spec.stdio.iter().enumerate() {
        if matches!(mode, None | Some(ProcessStdio::Inherit)) {
            let flags = unsafe { libc::fcntl(fd as RawFd, libc::F_GETFD) };
            if flags < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EBADF) {
                    return Err(error);
                }
                closed_stdio |= 1 << fd;
            } else if flags & libc::FD_CLOEXEC != 0 {
                closed_stdio |= 1 << fd;
            }
        }
    }
    let mut plan = vec![
        closed_stdio,
        spec.session as u8,
        spec.independent.map_or(0, |purpose| purpose as u8),
    ];
    plan.push(u8::from(spec.cwd.is_some()));
    if let Some(cwd) = &spec.cwd {
        field(&mut plan, cwd.as_os_str())?;
    }
    field(&mut plan, &spec.program)?;
    plan.extend_from_slice(
        &u32::try_from(spec.args.len())
            .map_err(io::Error::other)?
            .to_be_bytes(),
    );
    for arg in &spec.args {
        field(&mut plan, arg)?;
    }
    plan.extend_from_slice(
        &u32::try_from(env.len())
            .map_err(io::Error::other)?
            .to_be_bytes(),
    );
    for (key, value) in env {
        field(&mut plan, &key)?;
        field(&mut plan, &value)?;
    }
    if plan.len() > argument_limit()? {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }
    let mut message = Vec::with_capacity(plan.len() + 8);
    message.extend_from_slice(&(plan.len() as u64).to_be_bytes());
    message.extend_from_slice(&plan);
    Ok(message)
}

pub(super) fn spawn(spec: GroupCommand) -> io::Result<(Child, Exchange)> {
    let plan = encode(&spec)?;
    #[cfg(target_os = "linux")]
    let executable_path = std::path::PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let executable_path = std::env::current_exe()?;
    #[cfg(feature = "test-support")]
    let executable_path = if EXECUTABLE_ENTRYPOINT.load(std::sync::atomic::Ordering::Acquire) {
        executable_path
    } else {
        std::env::var_os("TYDE_TEST_PROCESS_EXECUTABLE")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| io::Error::other("test process launcher executable is not configured"))?
    };
    let executable = File::open(&executable_path)?;
    let executable = high_fd(executable.into())?;
    let mut actions = Actions::new()?;
    actions.duplicate(executable.as_raw_fd(), EXECUTABLE)?;
    let (control, peer) = UnixStream::pair()?;
    let control = UnixStream::from(high_fd(control.into())?);
    let peer = high_fd(peer.into())?;
    actions.duplicate(peer.as_raw_fd(), CONTROL)?;
    #[cfg(target_os = "linux")]
    let parent_pidfd =
        syscall(unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) as libc::c_int })?;
    #[cfg(target_os = "linux")]
    let parent_pidfd = high_fd(unsafe { OwnedFd::from_raw_fd(parent_pidfd) })?;
    #[cfg(target_os = "linux")]
    actions.duplicate(parent_pidfd.as_raw_fd(), PARENT)?;
    #[cfg(target_os = "linux")]
    let creator = {
        use std::os::unix::fs::OpenOptionsExt;
        high_fd(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
                .open("/proc/thread-self")?
                .into(),
        )?
    };
    #[cfg(target_os = "linux")]
    actions.duplicate(creator.as_raw_fd(), CREATOR)?;
    let mut child_fds = Vec::new();
    let mut parent_fds: [Option<OwnedFd>; 3] = [None, None, None];
    for (index, mode) in spec.stdio.iter().enumerate() {
        let fd = match mode.as_ref().unwrap_or(&ProcessStdio::Inherit) {
            ProcessStdio::Inherit => continue,
            ProcessStdio::File(file) => high_fd(file.try_clone()?.into())?,
            ProcessStdio::Null => high_fd(
                std::fs::OpenOptions::new()
                    .read(index == 0)
                    .write(index != 0)
                    .open("/dev/null")?
                    .into(),
            )?,
            ProcessStdio::Piped => {
                let (read, write) = pipe()?;
                if index == 0 {
                    parent_fds[index] = Some(write);
                    read
                } else {
                    parent_fds[index] = Some(read);
                    write
                }
            }
        };
        actions.duplicate(fd.as_raw_fd(), index as RawFd)?;
        child_fds.push(fd);
    }
    let stdin = parent_fds[0]
        .take()
        .map(|fd| tokio::process::ChildStdin::from_std(std::process::ChildStdin::from(fd)))
        .transpose()?;
    let stdout = parent_fds[1]
        .take()
        .map(|fd| tokio::process::ChildStdout::from_std(std::process::ChildStdout::from(fd)))
        .transpose()?;
    let stderr = parent_fds[2]
        .take()
        .map(|fd| tokio::process::ChildStderr::from_std(std::process::ChildStderr::from(fd)))
        .transpose()?;
    control.set_nonblocking(true)?;
    let control = tokio::net::UnixStream::from_std(control)?;
    #[cfg(target_os = "linux")]
    let path = c"/proc/self/fd/5";
    #[cfg(not(target_os = "linux"))]
    let path = cstring(executable_path.as_os_str())?;
    let argv = [
        path.as_ptr().cast_mut(),
        c"--tyde-process-exec".as_ptr().cast_mut(),
        std::ptr::null_mut(),
    ];
    // No provider environment or callbacks reach the shared-address-space
    // libc child. The trusted helper first closes all unrelated descriptors.
    let env = [std::ptr::null_mut()];
    let attributes = Attributes::new(spec.session)?;
    #[cfg(not(target_os = "linux"))]
    let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    let mut pid = 0;
    posix(unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            &actions.0,
            &attributes.0,
            argv.as_ptr(),
            env.as_ptr(),
        )
    })?;
    let pid = Pid {
        pid,
        reaped: false,
        handed_off: false,
    };
    #[cfg(target_os = "linux")]
    let ready = {
        let pidfd =
            syscall(unsafe { libc::syscall(libc::SYS_pidfd_open, pid.pid, 0) as libc::c_int })?;
        std::sync::Arc::new(Readiness {
            descriptor: AsyncFd::new(unsafe { OwnedFd::from_raw_fd(pidfd) })?,
        })
    };
    #[cfg(not(target_os = "linux"))]
    let ready = {
        let ready = std::sync::Arc::new(Readiness {
            changed: tokio::sync::Notify::new(),
        });
        let changed = std::sync::Arc::clone(&ready);
        tokio::spawn(async move {
            while exits.recv().await.is_some() {
                changed.changed.notify_one();
            }
        });
        ready
    };
    Ok((
        Child {
            pid,
            ready,
            stdin,
            stdout,
            stderr,
            status: None,
        },
        Exchange {
            socket: control,
            plan,
            independent: spec.independent.is_some(),
            spec,
        },
    ))
}

unsafe fn errno_location() -> *mut libc::c_int {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::__errno_location() }
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsafe { libc::__error() }
    }
}
fn close_unrelated_descriptors() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let directory_path = c"/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    let directory_path = c"/dev/fd";
    let directory = unsafe { libc::opendir(directory_path.as_ptr()) };
    if directory.is_null() {
        return Err(io::Error::last_os_error());
    }
    let own = unsafe { libc::dirfd(directory) };
    let mut fds = Vec::new();
    loop {
        unsafe {
            *errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            let errno = unsafe { *errno_location() };
            unsafe {
                libc::closedir(directory);
            }
            if errno != 0 {
                return Err(io::Error::from_raw_os_error(errno));
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if let Ok(name) = name.to_str()
            && let Ok(fd) = name.parse::<RawFd>()
            && fd
                >= if cfg!(target_os = "linux") {
                    EXECUTABLE
                } else {
                    4
                }
            && fd != own
            && !(cfg!(target_os = "linux") && fd == 6)
        {
            fds.push(fd);
        }
    }
    for fd in fds {
        syscall(unsafe { libc::close(fd) })?;
    }
    Ok(())
}

fn helper(control: &mut UnixStream, stage: &mut Stage) -> io::Result<()> {
    *stage = Stage::Descriptors;
    #[cfg(target_os = "linux")]
    let inherited = [CONTROL, PARENT, CREATOR];
    #[cfg(not(target_os = "linux"))]
    let inherited = [CONTROL];
    for fd in inherited {
        syscall(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }
    *stage = Stage::Parent;
    #[cfg(target_os = "linux")]
    {
        let mut credentials = MaybeUninit::<libc::ucred>::uninit();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        syscall(unsafe {
            libc::getsockopt(
                CONTROL,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut len,
            )
        })?;
        let credentials = unsafe { credentials.assume_init() };
        *stage = Stage::Parent;
        syscall(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) })?;
        // Socket credentials anchor the creator; the inherited pidfd still refers
        // to that original process even if its numeric PID has since been reused.
        let info = std::fs::read_to_string("/proc/self/fdinfo/4")?;
        let pid = info.lines().find_map(|line| {
            line.strip_prefix("Pid:")
                .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
        });
        if pid != Some(credentials.pid) {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        let mut parent = libc::pollfd {
            fd: PARENT,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = syscall(unsafe { libc::poll(&mut parent, 1, 0) })?;
        if ready != 0 || credentials.pid != unsafe { libc::getppid() } {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        // The process pidfd cannot detect a creating thread that exited before
        // PR_SET_PDEATHSIG. This procfs anchor never follows a reused TID.
        let creator =
            std::fs::read_to_string("/proc/self/fd/6/status").map_err(|error| {
                match error.raw_os_error() {
                    Some(libc::ENOENT | libc::ESRCH) => io::Error::from_raw_os_error(libc::ESRCH),
                    _ => error,
                }
            })?;
        let creator_process = creator.lines().find_map(|line| {
            line.strip_prefix("Tgid:")
                .and_then(|value| value.trim().parse::<libc::pid_t>().ok())
        });
        let creator_state = creator.lines().find_map(|line| {
            line.strip_prefix("State:")
                .and_then(|value| value.trim().chars().next())
        });
        if creator_process != Some(credentials.pid)
            || matches!(creator_state, None | Some('Z' | 'X' | 'x'))
        {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut parent = 0 as libc::pid_t;
        let mut length = std::mem::size_of_val(&parent) as libc::socklen_t;
        syscall(unsafe {
            libc::getsockopt(
                CONTROL,
                0,
                libc::LOCAL_PEERPID,
                (&mut parent as *mut libc::pid_t).cast(),
                &mut length,
            )
        })?;
        if parent != unsafe { libc::getppid() } {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
    }
    *stage = Stage::Descriptors;
    close_unrelated_descriptors()?;
    control.write_all(READY)?;
    *stage = Stage::Plan;
    let mut len = [0; 8];
    control.read_exact(&mut len)?;
    let len = usize::try_from(u64::from_be_bytes(len)).map_err(io::Error::other)?;
    if len > argument_limit()? {
        return Err(io::Error::from_raw_os_error(libc::E2BIG));
    }
    let mut plan = vec![0; len];
    control.read_exact(&mut plan)?;
    fn number(input: &mut &[u8]) -> io::Result<usize> {
        let value = input
            .get(..4)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        let count = u32::from_be_bytes(value.try_into().map_err(io::Error::other)?) as usize;
        *input = &input[4..];
        Ok(count)
    }
    fn field(input: &mut &[u8]) -> io::Result<OsString> {
        let count = number(input)?;
        let value = input
            .get(..count)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        let value = OsString::from_vec(value.to_vec());
        cstring(&value)?;
        *input = &input[count..];
        Ok(value)
    }
    let (closed_stdio, input) = plan
        .split_first()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
    if closed_stdio & !7 != 0 {
        return Err(io::Error::from_raw_os_error(libc::EPROTO));
    }
    let (session, input) = input
        .split_first()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
    let session = match session {
        0 => SessionMode::Group,
        1 => SessionMode::New,
        2 => SessionMode::Terminal,
        _ => return Err(io::Error::from_raw_os_error(libc::EPROTO)),
    };
    let (independent, input) = input
        .split_first()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
    if *independent > 2 {
        return Err(io::Error::from_raw_os_error(libc::EPROTO));
    }
    *stage = Stage::Session;
    if !matches!(session, SessionMode::Group) {
        syscall(unsafe { libc::setsid() })?;
        if matches!(session, SessionMode::Terminal) {
            #[cfg(target_os = "linux")]
            let request = libc::TIOCSCTTY;
            #[cfg(not(target_os = "linux"))]
            let request = libc::c_ulong::from(libc::TIOCSCTTY);
            syscall(unsafe { libc::ioctl(0, request, 0) })?;
        }
    }
    *stage = Stage::Plan;
    let (cwd_present, input) = input
        .split_first()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
    let mut input = input;
    let cwd = match cwd_present {
        0 => None,
        1 => Some(field(&mut input)?),
        _ => return Err(io::Error::from_raw_os_error(libc::EPROTO)),
    };
    let program = field(&mut input)?;
    let mut command = std::process::Command::new(program);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let count = number(&mut input)?;
    for _ in 0..count {
        command.arg(field(&mut input)?);
    }
    command.env_clear();
    let count = number(&mut input)?;
    for _ in 0..count {
        let key = field(&mut input)?;
        let value = field(&mut input)?;
        command.env(key, value);
    }
    if !input.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::EPROTO));
    }
    // Rust's helper runtime repairs closed standard fds on its own first
    // exec. Restore the requested inheritance immediately before target exec.
    *stage = Stage::Descriptors;
    for fd in 0..3 {
        if closed_stdio & (1 << fd) != 0 && unsafe { libc::close(fd) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(error);
            }
        }
    }
    if *independent != 0 {
        *stage = Stage::Parent;
        control.write_all(b"TYHO")?;
        let mut commit = [0];
        control.read_exact(&mut commit)?;
        if commit != *b"C" {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        if *independent == 1
            && unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } == libc::SIG_ERR
        {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        {
            syscall(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0) })?;
            let mut parent = libc::pollfd {
                fd: PARENT,
                events: libc::POLLIN,
                revents: 0,
            };
            if syscall(unsafe { libc::poll(&mut parent, 1, 0) })? != 0 {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
        }
    }
    *stage = Stage::Exec;
    Err(command.exec())
}

/// Dispatch before runtime, GUI, logging, or process-environment initialization.
pub fn exec_helper_entry() {
    #[cfg(feature = "test-support")]
    EXECUTABLE_ENTRYPOINT.store(true, std::sync::atomic::Ordering::Release);
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(HELPER_MODE)) {
        return;
    }
    if args.next().is_some() {
        std::process::exit(126);
    }
    if unsafe { libc::fcntl(CONTROL, libc::F_GETFD) } < 0 {
        std::process::exit(126);
    }
    #[cfg(target_os = "linux")]
    let socket_options = [
        (libc::SO_TYPE, libc::SOCK_STREAM),
        (libc::SO_DOMAIN, libc::AF_UNIX),
    ];
    #[cfg(not(target_os = "linux"))]
    let socket_options = [(libc::SO_TYPE, libc::SOCK_STREAM)];
    for (option, expected) in socket_options {
        let mut value = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                CONTROL,
                libc::SOL_SOCKET,
                option,
                (&mut value as *mut libc::c_int).cast(),
                &mut len,
            )
        } != 0
            || value != expected
        {
            std::process::exit(126);
        }
    }
    let mut control = unsafe { UnixStream::from_raw_fd(CONTROL) };
    let mut stage = Stage::Descriptors;
    if let Err(error) = helper(&mut control, &mut stage) {
        let mut failure = Vec::from(*ERROR);
        failure.push(stage as u8);
        let errno = match error.raw_os_error() {
            Some(errno) => errno,
            None => libc::EPROTO,
        };
        failure.extend_from_slice(&errno.to_be_bytes());
        if control.write_all(&failure).is_err() {
            std::process::exit(125);
        }
    }
    std::process::exit(126);
}
