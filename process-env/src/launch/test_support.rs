//! Real kernel/process instrumentation for protocol-level launch regressions.
use std::io;
use std::mem::{MaybeUninit, offset_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use super::{AsyncCommandGroup, LaunchPhase, ProcessOwner, ProcessStdio};

#[derive(Clone, Copy, Debug)]
enum Observation {
    Returned,
    Cancelled,
    Reaped,
    Failed,
    Child(u32),
}

#[derive(Clone, Debug)]
pub(super) struct Probe {
    installed: mpsc::UnboundedSender<io::Result<(OwnedFd, u32)>>,
    observations: mpsc::UnboundedSender<Observation>,
    taken: Arc<AtomicBool>,
}
impl Probe {
    pub(super) fn phase(&self, phase: LaunchPhase) {
        eprintln!("LAUNCH OBSERVATION phase={phase:?}");
        let value = match phase {
            LaunchPhase::SpawnReturned => Some(Observation::Returned),
            LaunchPhase::Cancelled => Some(Observation::Cancelled),
            LaunchPhase::CleanupFinished => Some(Observation::Reaped),
            LaunchPhase::Failed | LaunchPhase::CleanupFailed => Some(Observation::Failed),
            LaunchPhase::Registered
            | LaunchPhase::WorkerStarted
            | LaunchPhase::SpawnStarted
            | LaunchPhase::CleanupStarted
            | LaunchPhase::HandoffRequested
            | LaunchPhase::HandoffAuthorized
            | LaunchPhase::HelperPrepared => None,
        };
        if let Some(value) = value {
            let _ = self.observations.send(value);
        }
    }
    pub(super) fn child(&self, pid: Option<u32>) {
        if let Some(pid) = pid {
            let _ = self.observations.send(Observation::Child(pid));
        }
    }
    pub(super) fn install(&self) -> io::Result<()> {
        fn statement(code: u16, k: u32) -> libc::sock_filter {
            libc::sock_filter {
                code,
                jt: 0,
                jf: 0,
                k,
            }
        }
        let numbers = [
            libc::SYS_clone,
            libc::SYS_clone3,
            libc::SYS_execve,
            libc::SYS_prctl,
            libc::SYS_setpgid,
        ];
        let mut code = vec![statement(
            (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            offset_of!(libc::seccomp_data, nr) as u32,
        )];
        let mut notify = |number: libc::c_long| {
            code.push(libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: number as u32,
            });
            code.push(statement(
                (libc::BPF_RET | libc::BPF_K) as u16,
                libc::SECCOMP_RET_USER_NOTIF,
            ));
        };
        for number in numbers {
            notify(number);
        }
        #[cfg(target_arch = "x86_64")]
        {
            notify(libc::SYS_fork);
            notify(libc::SYS_vfork);
        }
        code.push(statement(
            (libc::BPF_RET | libc::BPF_K) as u16,
            libc::SECCOMP_RET_ALLOW,
        ));
        let filter = libc::sock_fprog {
            len: u16::try_from(code.len()).map_err(io::Error::other)?,
            filter: code.as_mut_ptr(),
        };
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let listener = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
                &filter,
            )
        };
        if listener < 0 {
            let error = io::Error::last_os_error();
            let _ = self.installed.send(Err(io::Error::new(
                error.kind(),
                "kernel launch observer unavailable",
            )));
            return Err(error);
        }
        let listener = unsafe { OwnedFd::from_raw_fd(listener as libc::c_int) };
        self.installed
            .send(Ok(
                (listener, unsafe { libc::syscall(libc::SYS_gettid) } as u32)
            ))
            .map_err(|_| io::Error::other("launch observer receiver closed"))
    }
}

tokio::task_local! { pub(super) static CURRENT_PROBE: Probe; }

pub(super) fn current_probe() -> Option<Probe> {
    CURRENT_PROBE
        .try_with(|probe| {
            if probe.taken.swap(true, Ordering::Relaxed) {
                None
            } else {
                Some(probe.clone())
            }
        })
        .ok()
        .flatten()
}

pub enum OperationResult<T> {
    Finished(T),
    Cancelled,
}

pub struct Operation<T> {
    owner: ProcessOwner,
    task: Option<tokio::task::JoinHandle<T>>,
}
impl<T> Operation<T> {
    pub fn cancel(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
    pub async fn finish(mut self) -> io::Result<OperationResult<T>> {
        let task = self
            .task
            .take()
            .ok_or_else(|| io::Error::other("operation already consumed"))?;
        match task.await {
            Ok(value) => Ok(OperationResult::Finished(value)),
            Err(error) if error.is_cancelled() => Ok(OperationResult::Cancelled),
            Err(_) => Err(io::Error::other("instrumented operation task failed")),
        }
    }
}
impl<T> Drop for Operation<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.owner.kill_all();
            task.abort();
        }
    }
}

pub struct Installation {
    installed: mpsc::UnboundedReceiver<io::Result<(OwnedFd, u32)>>,
    observations: mpsc::UnboundedReceiver<Observation>,
}
impl Installation {
    pub async fn ready(mut self) -> io::Result<Observer> {
        let (listener, owner_tid) = self
            .installed
            .recv()
            .await
            .ok_or_else(|| io::Error::other("operation did not install a native observer"))??;
        Ok(Observer {
            owner_tid,
            listener: AsyncFd::new(listener)?,
            observations: self.observations,
        })
    }
}
fn channels() -> (Probe, Installation) {
    let (installed, receiver) = mpsc::unbounded_channel();
    let (observations, events) = mpsc::unbounded_channel();
    (
        Probe {
            installed,
            observations,
            taken: Arc::new(AtomicBool::new(false)),
        },
        Installation {
            installed: receiver,
            observations: events,
        },
    )
}

pub async fn observe<F>(future: F) -> io::Result<(Operation<F::Output>, Observer)>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let future = Box::pin(future);
    let (probe, installation) = channels();
    let owner = ProcessOwner::default();
    let worker_owner = owner.clone();
    let task =
        tokio::spawn(async move { CURRENT_PROBE.scope(probe, worker_owner.scope(future)).await });
    let operation = Operation {
        owner,
        task: Some(task),
    };
    Ok((operation, installation.ready().await?))
}

#[derive(Clone)]
pub struct Fixture {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    probe: Probe,
    env: Vec<(String, String)>,
}
impl std::fmt::Debug for Fixture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeProcessFixture")
            .finish_non_exhaustive()
    }
}
impl Fixture {
    pub fn new(program: &str, args: &[&str], cwd: &Path) -> (Self, Installation) {
        let (probe, installation) = channels();
        (
            Self {
                program: program.to_owned(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                cwd: cwd.to_owned(),
                probe,
                env: Vec::new(),
            },
            installation,
        )
    }
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }
    pub async fn run(self) -> Result<(), String> {
        let mut command = super::command(&self.program)?;
        command
            .args(&self.args)
            .envs(self.env)
            .current_dir(self.cwd)
            .stdin(ProcessStdio::Null)
            .stdout(ProcessStdio::Null)
            .stderr(ProcessStdio::Null);
        command.probe = Some(self.probe);
        let mut child = command
            .group_spawn()
            .await
            .map_err(|error| format!("native fixture spawn failed: {error}"))?;
        let status = child
            .wait()
            .await
            .map_err(|error| format!("native fixture wait failed: {error}"))?;
        if !status.success() {
            return Err("native fixture exited unsuccessfully".to_owned());
        }
        Ok(())
    }
}

pub struct Creation {
    id: u64,
    pub shared_vm: bool,
    pub vfork: bool,
}
pub struct Observer {
    owner_tid: u32,
    listener: AsyncFd<OwnedFd>,
    observations: mpsc::UnboundedReceiver<Observation>,
}
#[derive(Default)]
pub struct Completion {
    pub native_returned: bool,
    pub cancelled: bool,
    pub child_pids: Vec<u32>,
}
impl Observer {
    async fn receive(listener: &AsyncFd<OwnedFd>) -> io::Result<Option<libc::seccomp_notif>> {
        loop {
            let mut ready = listener.readable().await?;
            // EPOLLHUP is readable to AsyncFd but is not a notification. The
            // kernel returns ENOENT at EOF; retrying that always-ready fd
            // starves queued observations. Only the channel proves reaping.
            if ready.ready().is_read_closed() {
                eprintln!("LAUNCH OBSERVATION phase=ListenerClosed");
                return Ok(None);
            }
            let mut request = MaybeUninit::<libc::seccomp_notif>::zeroed();
            let result = unsafe {
                libc::ioctl(
                    listener.as_raw_fd(),
                    libc::_IOWR::<libc::seccomp_notif>(b'!' as u32, 0),
                    request.as_mut_ptr(),
                )
            };
            if result == 0 {
                ready.clear_ready();
                return Ok(Some(unsafe { request.assume_init() }));
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENOENT | libc::EINTR) => {
                    ready.clear_ready();
                }
                _ => return Err(error),
            }
        }
    }
    fn respond(&self, id: u64) -> io::Result<()> {
        let response = libc::seccomp_notif_resp {
            id,
            val: 0,
            error: 0,
            flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
        };
        if unsafe {
            libc::ioctl(
                self.listener.as_raw_fd(),
                libc::_IOWR::<libc::seccomp_notif_resp>(b'!' as u32, 1),
                &response,
            )
        } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        }
    }
    fn creation(request: &libc::seccomp_notif) -> io::Result<Option<Creation>> {
        let nr = request.data.nr as libc::c_long;
        let mut flags = request.data.args[0];
        let creation = nr == libc::SYS_clone || nr == libc::SYS_clone3;
        #[cfg(target_arch = "x86_64")]
        let creation = creation || nr == libc::SYS_fork || nr == libc::SYS_vfork;
        if !creation {
            return Ok(None);
        }
        if nr == libc::SYS_clone3 {
            let local = libc::iovec {
                iov_base: (&mut flags as *mut u64).cast(),
                iov_len: 8,
            };
            let remote = libc::iovec {
                iov_base: request.data.args[0] as usize as *mut libc::c_void,
                iov_len: 8,
            };
            if unsafe {
                libc::process_vm_readv(request.pid as libc::pid_t, &local, 1, &remote, 1, 0)
            } != 8
            {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(target_arch = "x86_64")]
        if nr == libc::SYS_fork {
            flags = 0;
        }
        #[cfg(target_arch = "x86_64")]
        if nr == libc::SYS_vfork {
            flags = (libc::CLONE_VM | libc::CLONE_VFORK) as u64;
        }
        Ok(
            (flags & libc::CLONE_THREAD as u64 == 0).then_some(Creation {
                id: request.id,
                shared_vm: flags & libc::CLONE_VM as u64 != 0,
                vfork: flags & libc::CLONE_VFORK as u64 != 0,
            }),
        )
    }
    pub async fn hold_creation(&self) -> io::Result<Creation> {
        loop {
            let request = Self::receive(&self.listener)
                .await?
                .ok_or_else(|| io::Error::other("native launch ended before kernel creation"))?;
            if request.pid == self.owner_tid
                && let Some(creation) = Self::creation(&request)?
            {
                return Ok(creation);
            }
            self.respond(request.id)?;
        }
    }
    pub fn release(&self, creation: Creation) -> io::Result<()> {
        self.respond(creation.id)
    }
    pub async fn until_reaped(mut self) -> io::Result<Completion> {
        let mut result = Completion::default();
        let mut listener_open = true;
        loop {
            tokio::select! {
                observation=self.observations.recv()=>match observation {
                    Some(Observation::Returned)=>result.native_returned=true,
                    Some(Observation::Cancelled)=>result.cancelled=true,
                    Some(Observation::Child(pid))=>result.child_pids.push(pid),
                    Some(Observation::Reaped)=>return Ok(result),
                    Some(Observation::Failed)=>{},
                    None=>return Err(io::Error::other("launch fixture ended before reaping")),
                },
                request=Self::receive(&self.listener), if listener_open=>{
                    let Some(request)=request? else {listener_open=false;continue;};
                    if request.pid == self.owner_tid
                        && let Some(creation)=Self::creation(&request)?
                        && (!creation.shared_vm || !creation.vfork) {
                        return Err(io::Error::other("native launcher retried with heavyweight process creation"));
                    }
                    self.respond(request.id)?;
                },
            }
        }
    }
}
