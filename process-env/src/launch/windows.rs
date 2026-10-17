use super::{GroupCommand, ProcessStdio};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{ExitStatus, Stdio};
use winapi::shared::winerror::ERROR_NO_MORE_FILES;
use winapi::um::{
    handleapi::INVALID_HANDLE_VALUE,
    ioapiset::{CreateIoCompletionPort, GetQueuedCompletionStatus},
    jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    },
    processthreadsapi::{OpenThread, ResumeThread},
    tlhelp32::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    },
    winbase::CREATE_SUSPENDED,
    winnt::{
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO,
        JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
        THREAD_SUSPEND_RESUME,
    },
};

fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn handle(raw: winapi::um::winnt::HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(raw.cast()) })
    }
}
struct Job {
    handle: OwnedHandle,
    port: OwnedHandle,
    handed_off: bool,
}
impl Job {
    fn new() -> io::Result<Self> {
        let job = handle(unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) })?;
        let port = handle(unsafe {
            CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1)
        })?;
        let mut associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: job.as_raw_handle().cast(),
            CompletionPort: port.as_raw_handle().cast(),
        };
        checked(unsafe {
            SetInformationJobObject(
                job.as_raw_handle().cast(),
                JobObjectAssociateCompletionPortInformation,
                (&mut associate as *mut JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                std::mem::size_of_val(&associate) as u32,
            )
        })?;
        let job = Self {
            handle: job,
            port,
            handed_off: false,
        };
        job.set_owned(true)?;
        Ok(job)
    }
    fn set_owned(&self, owned: bool) -> io::Result<()> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        if owned {
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        checked(unsafe {
            SetInformationJobObject(
                self.handle.as_raw_handle().cast(),
                JobObjectExtendedLimitInformation,
                (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        })
    }
    fn kill(&self) -> io::Result<()> {
        if self.handed_off {
            return Ok(());
        }
        checked(unsafe { TerminateJobObject(self.handle.as_raw_handle().cast(), 1) })
    }
    fn wait_for_tree(&self) -> io::Result<()> {
        if self.handed_off {
            return Ok(());
        }
        loop {
            let mut message = 0;
            let mut key = 0;
            let mut overlapped = std::ptr::null_mut();
            checked(unsafe {
                GetQueuedCompletionStatus(
                    self.port.as_raw_handle().cast(),
                    &mut message,
                    &mut key,
                    &mut overlapped,
                    u32::MAX,
                )
            })?;
            if message == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO {
                return Ok(());
            }
        }
    }
}

pub(super) struct Child {
    child: tokio::process::Child,
    job: Job,
}
impl Child {
    pub(super) fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.stdin.take()
    }
    pub(super) fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }
    pub(super) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }
    pub(super) fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        self.job.kill()
    }
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    pub(super) fn handoff(&mut self) -> io::Result<()> {
        self.job.set_owned(false)?;
        self.job.handed_off = true;
        Ok(())
    }
    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await?;
        self.job.kill()?;
        // This blocking native wait runs only on the dedicated owning thread,
        // after the caller released the child; never on a host runtime worker.
        self.job.wait_for_tree()?;
        Ok(status)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if let Err(error) = self.job.kill() {
            tracing::error!(error_kind=?error.kind(),"Windows process tree cleanup failed");
        }
    }
}

fn resume_primary_thread(pid: u32) -> io::Result<()> {
    let snapshot = handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    let mut available = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &mut entry) };
    while available != 0 {
        if entry.th32OwnerProcessID == pid {
            let thread =
                handle(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        available = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &mut entry) };
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
        Err(io::Error::other(
            "suspended process primary thread was not found",
        ))
    } else {
        Err(error)
    }
}
pub(super) fn spawn(spec: GroupCommand) -> io::Result<Child> {
    let mut command = tokio::process::Command::new(spec.program);
    command.args(spec.args);
    for (key, value) in spec.env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    if let Some(cwd) = spec.cwd {
        command.current_dir(cwd);
    }
    let [stdin, stdout, stderr] =
        spec.stdio
            .map(|mode| match mode.unwrap_or(ProcessStdio::Inherit) {
                ProcessStdio::Inherit => Ok(Stdio::inherit()),
                ProcessStdio::Null => Ok(Stdio::null()),
                ProcessStdio::Piped => Ok(Stdio::piped()),
                ProcessStdio::File(file) => file.try_clone().map(Stdio::from),
            });
    command
        .stdin(stdin?)
        .stdout(stdout?)
        .stderr(stderr?)
        .creation_flags(CREATE_SUSPENDED)
        .kill_on_drop(false);
    let job = Job::new()?;
    let mut child = command.spawn()?;
    let result = (|| {
        let raw = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("new suspended process has no handle"))?;
        checked(unsafe {
            AssignProcessToJobObject(job.handle.as_raw_handle().cast(), raw.cast())
        })?;
        resume_primary_thread(
            child
                .id()
                .ok_or_else(|| io::Error::other("new suspended process has no PID"))?,
        )
    })();
    if let Err(error) = result {
        // Assignment/resume failure cannot leave a suspended process behind.
        child.start_kill()?;
        return Err(error);
    }
    Ok(Child { child, job })
}
